//! Install-step execution (`04-phased-plan.md` step 19; extended by
//! `memory/acpx/gen/plans/agents-install-runtime`): given an agent's
//! declared distribution, either pre-fetch an on-demand `npx`/`uvx`
//! package (and write a ready marker under `~/.acpx/adapters/<id>/`) or
//! download+extract a platform-matched `binary` archive into that same
//! adapters tree.
//!
//! Per `05-open-risks.md`'s cross-platform installability notes: the
//! `binary` archive's format (`.tar.gz`/`.tgz` vs `.zip`) is sniffed from
//! the `archive` URL itself, never assumed from the platform key, and the
//! registry's `cmd` string is treated as an opaque, already-correct path
//! for its own platform -- joined with the extraction dir and returned
//! as-is, never normalized or suffixed.

use crate::index::Agent;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Filename written under `adapters/<agent_id>/` when an npx/uvx package
/// pre-fetch (or binary extract) has succeeded. `acpx-core::detect` treats
/// this as "Installed" for runtime-distributed agents.
pub const READY_MARKER_NAME: &str = ".ready";

/// Build the `Command` for `program`, resolving Windows `.cmd`/`.bat`
/// launcher stubs the same way `acpx-conductor::process::base_command`
/// does for the real agent spawn path (see that function's doc comment for
/// the full `os error 193` background -- `npm`/`npx`/`uvx` ship no bare
/// `.exe` on Windows PATH, so `Command::new("npm")` there fails the same
/// way `code` did before `editor_detect.rs`'s `a0cbde2e` fix). Used for
/// this crate's own pre-fetch/runtime-probe spawns so an install attempt
/// doesn't fail with an opaque os-error-193 before ever reaching the real
/// agent spawn in `acpx-conductor`.
///
/// Unix is unchanged: returns `Command::new(program)` as before.
fn runtime_command(program: &str) -> Command {
    #[cfg(windows)]
    {
        if let Some((resolved, is_script_launcher)) = resolve_windows_runtime_program(program) {
            if is_script_launcher {
                let mut cmd = Command::new("cmd");
                cmd.arg("/C").arg(resolved);
                return cmd;
            }
            return Command::new(resolved);
        }
    }
    Command::new(program)
}

#[cfg(windows)]
fn resolve_windows_runtime_program(program: &str) -> Option<(PathBuf, bool)> {
    let path = Path::new(program);
    let is_script_ext = |ext: &str| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat");
    if path.extension().is_some() || path.parent().filter(|p| !p.as_os_str().is_empty()).is_some() {
        let is_script = path
            .extension()
            .and_then(|e| e.to_str())
            .map(is_script_ext)
            .unwrap_or(false);
        return Some((path.to_path_buf(), is_script));
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for ext in ["cmd", "bat", "exe"] {
            let candidate = dir.join(format!("{program}.{ext}"));
            if candidate.is_file() {
                return Some((candidate, is_script_ext(ext)));
            }
        }
    }
    None
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("agent {0} declares no supported distribution method")]
    NoDistribution(String),
    #[error(
        "required runtime `{runtime}` not found on PATH (needed for agent `{agent_id}`'s {method} distribution)"
    )]
    RuntimeMissing {
        runtime: &'static str,
        agent_id: String,
        method: &'static str,
    },
    #[error("failed to pre-fetch {method} package `{package}` for agent `{agent_id}`: {detail}")]
    PackageFetchFailed {
        agent_id: String,
        method: &'static str,
        package: String,
        detail: String,
    },
    #[error(
        "agent `{agent_id}` has no binary distribution entry for host platform `{platform_key}`"
    )]
    UnsupportedPlatform {
        agent_id: String,
        platform_key: String,
    },
    #[error("failed to download archive from {url}: {source}")]
    Download {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("archive download returned HTTP {status} for {url}")]
    DownloadStatus { url: String, status: u16 },
    #[error("unrecognized archive extension for url `{0}` -- expected one of .tar.gz, .tgz, .zip")]
    UnknownArchiveFormat(String),
    #[error("failed to extract tar.gz archive into {dir}: {source}")]
    ExtractTarGz {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to extract zip archive into {dir}: {source}")]
    ExtractZip {
        dir: PathBuf,
        #[source]
        source: zip::result::ZipError,
    },
    #[error("failed to create adapter directory {0}: {1}")]
    CreateDir(PathBuf, #[source] std::io::Error),
    #[error("failed to write ready marker {0}: {1}")]
    ReadyMarkerWrite(PathBuf, #[source] std::io::Error),
}

/// Outcome of a successful [`install`]/[`install_into`] call, distinct per
/// distribution method so `acpx-core` can build `agents/status` semantics on
/// top of it (out of scope for this crate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// `npx`/`uvx`: runtime was confirmed **and** the package was
    /// pre-fetched; a ready marker was written under `adapters/<id>/`.
    PackageReady {
        runtime: &'static str,
        package: String,
        marker: PathBuf,
    },
    /// `binary`: the archive was downloaded and extracted. `cmd` is the
    /// opaque, platform-correct executable path joined with `dir`, ready to
    /// spawn as-is.
    Extracted { dir: PathBuf, cmd: PathBuf },
}

/// Resolve and run the install step for one agent's preferred distribution
/// method, extracting `binary` archives into the default
/// `~/.acpx/adapters/` root.
pub async fn install(agent: &Agent) -> Result<InstallOutcome, InstallError> {
    install_into(agent, &default_adapters_dir()).await
}

/// Same as [`install`] but with an explicit adapters root -- exists mainly
/// so tests can point extraction at a tempdir instead of `~/.acpx/adapters/`.
pub async fn install_into(
    agent: &Agent,
    adapters_root: &Path,
) -> Result<InstallOutcome, InstallError> {
    let method = agent
        .distribution
        .preferred_method()
        .ok_or_else(|| InstallError::NoDistribution(agent.id.clone()))?;

    match method {
        "npx" => install_npx(agent, adapters_root).await,
        "uvx" => install_uvx(agent, adapters_root).await,
        "binary" => install_binary(agent, adapters_root).await,
        // preferred_method() only ever returns one of the three above.
        _ => unreachable!("Distribution::preferred_method returned an unhandled method"),
    }
}

/// Default adapters root: `~/.acpx/adapters/`.
pub fn default_adapters_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".acpx")
        .join("adapters")
}

/// Path of the ready marker for `agent_id` under `adapters_root`.
pub fn ready_marker_path(adapters_root: &Path, agent_id: &str) -> PathBuf {
    adapters_root.join(agent_id).join(READY_MARKER_NAME)
}

/// True when a successful install has left a ready marker for this agent.
pub fn is_package_ready(adapters_root: &Path, agent_id: &str) -> bool {
    ready_marker_path(adapters_root, agent_id).is_file()
}

/// Write (or overwrite) the ready marker after a successful package pre-fetch
/// or binary extract.
pub fn write_ready_marker(
    adapters_root: &Path,
    agent_id: &str,
    method: &str,
    package: &str,
) -> Result<PathBuf, InstallError> {
    let dir = adapters_root.join(agent_id);
    std::fs::create_dir_all(&dir).map_err(|e| InstallError::CreateDir(dir.clone(), e))?;
    let path = dir.join(READY_MARKER_NAME);
    let body = format!(
        "method={method}\npackage={package}\nwritten_unix_secs={}\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    std::fs::write(&path, body).map_err(|e| InstallError::ReadyMarkerWrite(path.clone(), e))?;
    Ok(path)
}

async fn install_npx(agent: &Agent, adapters_root: &Path) -> Result<InstallOutcome, InstallError> {
    check_runtime("node", agent, "npx").await?;
    check_runtime("npm", agent, "npx").await?;
    let package = agent
        .distribution
        .npx
        .as_ref()
        .map(|n| n.package.clone())
        .ok_or_else(|| InstallError::NoDistribution(agent.id.clone()))?;
    prefetch_npx_package(agent, &package, adapters_root).await?;
    let marker = write_ready_marker(adapters_root, &agent.id, "npx", &package)?;
    Ok(InstallOutcome::PackageReady {
        runtime: "node+npm",
        package,
        marker,
    })
}

async fn install_uvx(agent: &Agent, adapters_root: &Path) -> Result<InstallOutcome, InstallError> {
    check_runtime("uv", agent, "uvx").await?;
    let package = agent
        .distribution
        .uvx
        .as_ref()
        .map(|u| u.package.clone())
        .ok_or_else(|| InstallError::NoDistribution(agent.id.clone()))?;
    prefetch_uvx_package(agent, &package).await?;
    let marker = write_ready_marker(adapters_root, &agent.id, "uvx", &package)?;
    Ok(InstallOutcome::PackageReady {
        runtime: "uv",
        package,
        marker,
    })
}

/// Default hard ceiling on one package pre-fetch (`npm install` / `uvx`).
/// Network + first-time cache populate can be slow (especially the Codex ACP
/// package on a cold npm cache), so the production default is five minutes;
/// the operation still fails closed rather than hanging the gateway forever.
const DEFAULT_PACKAGE_PREFETCH_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(300);

/// Allow operators/tests to tune the cold-cache ceiling without rebuilding.
/// Keep the range bounded so an accidental environment value cannot turn a
/// settings click into an unbounded task. Values below 30s are clamped up;
/// values above 15m are clamped down.
fn package_prefetch_timeout() -> std::time::Duration {
    let seconds = std::env::var("ACPX_PACKAGE_PREFETCH_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|seconds| seconds.clamp(30, 900));
    seconds
        .map(std::time::Duration::from_secs)
        .unwrap_or(DEFAULT_PACKAGE_PREFETCH_TIMEOUT)
}

/// Pre-fetch an npm package into `adapters_root/<agent_id>/` via
/// `npm install --prefix …`. We deliberately do **not** run
/// `npx -y <package> --version`: that executes the adapter binary (many
/// ACP adapters treat unknown flags as a hard error or hang waiting for
/// stdio ACP), whereas `npm install --prefix` only populates
/// `node_modules` / the npm cache so later `npx -y` / spawn is warm.
async fn prefetch_npx_package(
    agent: &Agent,
    package: &str,
    adapters_root: &Path,
) -> Result<(), InstallError> {
    let package_owned = package.to_string();
    let agent_id = agent.id.clone();
    let prefix = adapters_root.join(&agent.id);
    std::fs::create_dir_all(&prefix).map_err(|e| InstallError::CreateDir(prefix.clone(), e))?;
    let npm_bin = resolve_npm_bin();
    // npm is typically a #!/usr/bin/env node script — ensure the same
    // prefix's bin/ is first on PATH so we never mix global node + bundled npm.
    let path_for_npm = npm_bin_dir_path_prefix(&npm_bin);
    let probe = tokio::task::spawn_blocking(move || {
        let mut cmd = runtime_command(&npm_bin);
        cmd.args([
            "install",
            "--no-save",
            "--no-audit",
            "--no-fund",
            "--prefix",
        ])
        .arg(&prefix)
        .arg(&package_owned)
        // MUST null stderr (not pipe-without-read): a full pipe buffer
        // stalls npm on write while we block on status() — classic hang.
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        if let Some(p) = path_for_npm {
            cmd.env("PATH", p);
        }
        cmd.status().map(|status| status.success()).unwrap_or(false)
    });
    // Nested Result: timeout Elapsed vs join/panic vs exit status — all
    // collapse to PackageFetchFailed (operator re-runs install; no retry
    // policy here). spawn_blocking work is NOT cancelled on timeout.
    let ok = match tokio::time::timeout(package_prefetch_timeout(), probe).await {
        Ok(Ok(success)) => success,
        Ok(Err(_)) | Err(_) => false,
    };
    if ok {
        Ok(())
    } else {
        Err(InstallError::PackageFetchFailed {
            agent_id,
            method: "npx",
            package: package.to_string(),
            detail: "npm install --prefix <adapters/id> <package> failed or timed out".to_string(),
        })
    }
}

async fn prefetch_uvx_package(agent: &Agent, package: &str) -> Result<(), InstallError> {
    let package_owned = package.to_string();
    let agent_id = agent.id.clone();
    let probe = tokio::task::spawn_blocking(move || {
        runtime_command("uvx")
            .args([&package_owned, "--help"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    });
    let ok = tokio::time::timeout(package_prefetch_timeout(), probe)
        .await
        .ok()
        .and_then(|joined| joined.ok())
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err(InstallError::PackageFetchFailed {
            agent_id,
            method: "uvx",
            package: package.to_string(),
            detail: "uvx <package> --help failed or timed out".to_string(),
        })
    }
}

/// Hard ceiling on one `<bin> --version` probe.
///
/// **Why this exists.** `check_runtime` previously ran
/// `std::process::Command::status()` -- a synchronous, blocking call --
/// directly inline inside this crate's `async fn install_into`, with no
/// timeout: a `--version` invocation that never returns (a broken/wrapped
/// binary, a stalled network-mounted `PATH` entry, a shell wrapper
/// blocking despite `Stdio::null()`) parked the calling tokio worker
/// thread forever, and unlike a plain hang in owned async code, this one
/// doesn't even show up as an await point another task could interleave
/// around -- it starves the whole worker thread for any other task
/// scheduled onto it. Now run via `spawn_blocking` (so it can't starve an
/// async worker thread) and bounded by this timeout (so a caller doesn't
/// wait forever even though the spawned blocking thread itself isn't
/// forcibly killed -- `std::process::Command` has no async cancellation
/// primitive to hook into here).
const RUNTIME_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Shells out to `<bin> --version` to confirm an on-demand runtime is
/// available, mirroring the check `acpx-core`'s `detect.rs` does for
/// `agents/status` (not shared code -- that module is owned by the main
/// agent).
/// Absolute npm for ACP: SNAPFLOW_ACP_NODE_HOME/bin/npm if set (bundled),
/// else bare `npm` (global-first via PATH).
fn resolve_npm_bin() -> String {
    if let Ok(home) = std::env::var("SNAPFLOW_ACP_NODE_HOME") {
        let p = std::path::Path::new(&home).join("bin").join("npm");
        if p.is_file() {
            return p.to_string_lossy().into_owned();
        }
    }
    "npm".to_string()
}

/// Absolute node for runtime checks when bundled home is set.
fn resolve_runtime_bin(bin: &'static str) -> String {
    if let Ok(home) = std::env::var("SNAPFLOW_ACP_NODE_HOME") {
        let p = std::path::Path::new(&home).join("bin").join(bin);
        if p.is_file() {
            return p.to_string_lossy().into_owned();
        }
    }
    bin.to_string()
}

/// PATH with npm's directory first (so `env node` resolves same-prefix).
fn npm_bin_dir_path_prefix(npm_bin: &str) -> Option<String> {
    let path = std::path::Path::new(npm_bin);
    let dir = path.parent()?;
    if !dir.is_absolute() {
        return None;
    }
    let rest = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths([dir.as_os_str(), rest.as_os_str()])
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

async fn check_runtime(
    bin: &'static str,
    agent: &Agent,
    method: &'static str,
) -> Result<(), InstallError> {
    let resolved = resolve_runtime_bin(bin);
    // Absolute npm still needs its bin/ on PATH for `env node` shebang.
    let path_for_bin = npm_bin_dir_path_prefix(&resolved);
    let probe = tokio::task::spawn_blocking(move || {
        let mut cmd = runtime_command(&resolved);
        cmd.arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(p) = path_for_bin {
            cmd.env("PATH", p);
        }
        cmd.status().map(|status| status.success()).unwrap_or(false)
    });
    let ok = match tokio::time::timeout(RUNTIME_PROBE_TIMEOUT, probe).await {
        Ok(Ok(success)) => success,
        Ok(Err(_)) | Err(_) => false,
    };

    if ok {
        Ok(())
    } else {
        Err(InstallError::RuntimeMissing {
            runtime: bin,
            agent_id: agent.id.clone(),
            method,
        })
    }
}

/// The registry's `<os>-<arch>` platform key for the host this process is
/// running on, e.g. `linux-x86_64`, `darwin-aarch64`, `windows-x86_64`. The
/// registry uses `darwin` (not Rust's `macos`) for the OS component.
pub fn host_platform_key() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    format!("{os}-{}", std::env::consts::ARCH)
}

async fn install_binary(
    agent: &Agent,
    adapters_root: &Path,
) -> Result<InstallOutcome, InstallError> {
    let binaries = agent
        .distribution
        .binary
        .as_ref()
        .ok_or_else(|| InstallError::NoDistribution(agent.id.clone()))?;

    let platform_key = host_platform_key();
    let dist = binaries
        .get(&platform_key)
        .ok_or_else(|| InstallError::UnsupportedPlatform {
            agent_id: agent.id.clone(),
            platform_key: platform_key.clone(),
        })?;

    let client = reqwest::Client::new();
    let archive_bytes = download(&client, &dist.archive).await?;

    let dest_dir = adapters_root.join(&agent.id);
    std::fs::create_dir_all(&dest_dir).map_err(|e| InstallError::CreateDir(dest_dir.clone(), e))?;

    extract_archive(&dist.archive, &archive_bytes, &dest_dir)?;

    // `cmd` is opaque per 05-open-risks.md: join as-is, never normalize
    // separators or append a platform suffix ourselves.
    let cmd = dest_dir.join(&dist.cmd);
    // Same ready marker as npx/uvx so detect can treat binary installs
    // uniformly when checking adapters/<id>/.ready (directory existence
    // alone still counts as Installed for binary in detect).
    let _ = write_ready_marker(adapters_root, &agent.id, "binary", &dist.cmd);
    Ok(InstallOutcome::Extracted { dir: dest_dir, cmd })
}

async fn download(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, InstallError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|source| InstallError::Download {
            url: url.to_string(),
            source,
        })?;

    let status = response.status();
    if !status.is_success() {
        return Err(InstallError::DownloadStatus {
            url: url.to_string(),
            status: status.as_u16(),
        });
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|source| InstallError::Download {
            url: url.to_string(),
            source,
        })?;
    Ok(bytes.to_vec())
}

/// Sniffs the archive format from the URL's extension (per
/// `05-open-risks.md`: Windows entries are inconsistently `.zip` vs
/// `.tar.gz`, so the platform key alone can't tell us the format) and
/// extracts into `dest`.
fn extract_archive(url: &str, bytes: &[u8], dest: &Path) -> Result<(), InstallError> {
    let lower = url.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        extract_tar_gz(bytes, dest)
    } else if lower.ends_with(".zip") {
        extract_zip(bytes, dest)
    } else {
        Err(InstallError::UnknownArchiveFormat(url.to_string()))
    }
}

fn extract_tar_gz(bytes: &[u8], dest: &Path) -> Result<(), InstallError> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(dest)
        .map_err(|source| InstallError::ExtractTarGz {
            dir: dest.to_path_buf(),
            source,
        })
}

fn extract_zip(bytes: &[u8], dest: &Path) -> Result<(), InstallError> {
    let cursor = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).map_err(|source| InstallError::ExtractZip {
        dir: dest.to_path_buf(),
        source,
    })?;
    archive
        .extract(dest)
        .map_err(|source| InstallError::ExtractZip {
            dir: dest.to_path_buf(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Distribution;
    use std::io::Write;

    fn test_agent(id: &str, distribution: Distribution) -> Agent {
        Agent {
            id: id.to_string(),
            name: id.to_string(),
            version: "0.0.0".to_string(),
            description: None,
            repository: None,
            website: None,
            authors: vec![],
            license: None,
            icon: None,
            distribution,
        }
    }

    /// Builds a one-entry `.tar.gz` in memory -- no filesystem, no network.
    fn make_tar_gz(entry_name: &str, contents: &[u8]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, entry_name, contents)
            .expect("append_data into in-memory tar builder");
        let tar_bytes = builder.into_inner().expect("finish in-memory tar");

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder
            .write_all(&tar_bytes)
            .expect("write tar bytes into gzip encoder");
        encoder.finish().expect("finish gzip encoder")
    }

    /// Builds a one-entry `.zip` in memory -- no filesystem, no network.
    fn make_zip(entry_name: &str, contents: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        let cursor = Cursor::new(&mut buf);
        let mut writer = zip::ZipWriter::new(cursor);
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        writer
            .start_file(entry_name, options)
            .expect("start_file in in-memory zip writer");
        writer
            .write_all(contents)
            .expect("write zip entry contents");
        writer.finish().expect("finish in-memory zip writer");
        buf
    }

    #[test]
    fn sniffs_and_extracts_tar_gz_by_url_extension() {
        let dest = tempfile::tempdir().unwrap();
        let archive = make_tar_gz("hello.txt", b"hi from tar.gz");
        extract_archive(
            "https://example.com/releases/agent-linux-x86_64.tar.gz",
            &archive,
            dest.path(),
        )
        .unwrap();
        let extracted = std::fs::read(dest.path().join("hello.txt")).unwrap();
        assert_eq!(extracted, b"hi from tar.gz");
    }

    #[test]
    fn sniffs_and_extracts_tgz_by_url_extension() {
        let dest = tempfile::tempdir().unwrap();
        let archive = make_tar_gz("hello.txt", b"hi from tgz");
        // Confirmed real case from 05-open-risks.md: e.g. cortex-code ships
        // a .tar.gz for windows-x86_64 -- the ".tgz" spelling must also be
        // recognized, independent of any platform assumption.
        extract_archive(
            "https://example.com/releases/agent-windows-x86_64.tgz",
            &archive,
            dest.path(),
        )
        .unwrap();
        let extracted = std::fs::read(dest.path().join("hello.txt")).unwrap();
        assert_eq!(extracted, b"hi from tgz");
    }

    #[test]
    fn sniffs_and_extracts_zip_by_url_extension() {
        let dest = tempfile::tempdir().unwrap();
        let archive = make_zip("agent.exe", b"hi from zip");
        // Per 05-open-risks.md: Windows archives are inconsistently .zip vs
        // .tar.gz -- this must be sniffed from the URL's own extension, not
        // assumed from a "windows" platform key.
        extract_archive(
            "https://example.com/releases/agent-windows-x86_64.zip",
            &archive,
            dest.path(),
        )
        .unwrap();
        let extracted = std::fs::read(dest.path().join("agent.exe")).unwrap();
        assert_eq!(extracted, b"hi from zip");
    }

    #[test]
    fn unknown_extension_is_rejected_without_guessing() {
        let dest = tempfile::tempdir().unwrap();
        let err = extract_archive(
            "https://example.com/releases/agent-linux-x86_64.rar",
            b"whatever",
            dest.path(),
        )
        .unwrap_err();
        assert!(matches!(err, InstallError::UnknownArchiveFormat(_)));
    }

    #[test]
    fn host_platform_key_uses_darwin_not_macos() {
        // Registry entries key on "darwin-*", never Rust's own "macos"
        // spelling for std::env::consts::OS.
        let key = host_platform_key();
        assert!(!key.starts_with("macos-"), "got {key}");
        assert!(key.contains('-'), "got {key}");
    }

    #[test]
    fn cmd_path_is_joined_opaquely_not_normalized() {
        // Regression guard for the open-risks note: real cmd strings mix
        // separators (e.g. "./goose-package\\goose.exe") -- must be joined
        // as-is, never rewritten or platform-suffixed by us.
        let dest = std::path::Path::new("/tmp/acpx-adapters/goose");
        let cmd = "./goose-package\\goose.exe";
        let joined = dest.join(cmd);
        assert_eq!(
            joined,
            std::path::PathBuf::from("/tmp/acpx-adapters/goose/./goose-package\\goose.exe")
        );
    }

    #[tokio::test]
    async fn check_runtime_reports_runtime_missing_for_absent_binary() {
        let agent = test_agent("unit-test-agent", Distribution::default());
        let err = check_runtime("definitely-not-a-real-binary-xyz", &agent, "npx")
            .await
            .unwrap_err();
        assert!(matches!(err, InstallError::RuntimeMissing { .. }));
    }

    #[test]
    fn ready_marker_round_trip_under_adapters_root() {
        let root = tempfile::tempdir().unwrap();
        assert!(!is_package_ready(root.path(), "claude-acp"));
        let path = write_ready_marker(root.path(), "claude-acp", "npx", "pkg@1").unwrap();
        assert!(path.ends_with(READY_MARKER_NAME));
        assert!(is_package_ready(root.path(), "claude-acp"));
        let body = std::fs::read_to_string(path).unwrap();
        assert!(body.contains("package=pkg@1"));
        assert!(body.contains("method=npx"));
    }

    #[tokio::test]
    async fn install_into_reports_unsupported_platform_for_unknown_binary_key() {
        use crate::index::BinaryDist;
        use std::collections::HashMap;

        let mut binaries = HashMap::new();
        binaries.insert(
            "some-platform-that-does-not-exist".to_string(),
            BinaryDist {
                archive: "https://example.com/nope.tar.gz".to_string(),
                cmd: "./nope".to_string(),
                args: vec![],
            },
        );
        let agent = test_agent(
            "binary-only-agent",
            Distribution {
                npx: None,
                uvx: None,
                binary: Some(binaries),
            },
        );

        let dest = tempfile::tempdir().unwrap();
        let err = install_into(&agent, dest.path()).await.unwrap_err();
        assert!(matches!(err, InstallError::UnsupportedPlatform { .. }));
    }
}
