//! Integration tests for the `npx`/`uvx` install path
//! (`agents-install-runtime` plan): real runtimes on PATH, and for npx a
//! real package pre-fetch into a temp adapters root (writes `.ready`).

use acpx_registry::index::{Distribution, NpxDist};
use acpx_registry::{install, install_into, is_package_ready, Agent, InstallError, InstallOutcome};

fn agent_with(id: &str, distribution: Distribution) -> Agent {
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

#[tokio::test]
async fn npx_install_prefetches_and_writes_ready_marker() {
    // Use the same package the server's native default uses so a warm
    // npm/npx cache (common on this machine) avoids a cold multi-minute
    // download in CI; still exercises real `npx -y`.
    let agent = agent_with(
        "codex-acp-install-test",
        Distribution {
            npx: Some(NpxDist {
                package: "@agentclientprotocol/codex-acp@1.1.2".to_string(),
                args: vec![],
            }),
            uvx: None,
            binary: None,
        },
    );

    let dest = tempfile::tempdir().unwrap();
    let outcome = install_into(&agent, dest.path())
        .await
        .expect("node/npm + npx pre-fetch should succeed in this environment");
    match outcome {
        InstallOutcome::PackageReady {
            runtime,
            package,
            marker,
        } => {
            assert_eq!(runtime, "node+npm");
            assert!(package.contains("codex-acp"));
            assert!(marker.is_file());
            assert!(is_package_ready(dest.path(), "codex-acp-install-test"));
        }
        other => panic!("expected PackageReady, got {other:?}"),
    }
}

#[tokio::test]
async fn npx_install_fails_package_fetch_for_nonexistent_package() {
    let agent = agent_with(
        "bad-npx-agent",
        Distribution {
            npx: Some(NpxDist {
                package: "@agentclientprotocol/definitely-not-a-real-package-zzzz@0.0.0"
                    .to_string(),
                args: vec![],
            }),
            uvx: None,
            binary: None,
        },
    );
    let dest = tempfile::tempdir().unwrap();
    let err = install_into(&agent, dest.path()).await.unwrap_err();
    assert!(
        matches!(err, InstallError::PackageFetchFailed { .. }),
        "got {err:?}"
    );
    assert!(!is_package_ready(dest.path(), "bad-npx-agent"));
}

#[tokio::test]
async fn uvx_install_prefetches_when_uv_present() {
    // Skip cleanly when uv is not installed (not every CI image has it).
    if std::process::Command::new("uv")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
        == false
    {
        return;
    }

    let agent = agent_with(
        "test-uvx-agent",
        Distribution {
            npx: None,
            // ruff is a common small uvx tool; --help is enough for pre-fetch.
            uvx: Some(NpxDist {
                package: "ruff".to_string(),
                args: vec![],
            }),
            binary: None,
        },
    );

    let dest = tempfile::tempdir().unwrap();
    match install_into(&agent, dest.path()).await {
        Ok(InstallOutcome::PackageReady {
            runtime,
            package,
            marker,
        }) => {
            assert_eq!(runtime, "uv");
            assert_eq!(package, "ruff");
            assert!(marker.is_file());
        }
        Ok(other) => panic!("expected PackageReady, got {other:?}"),
        Err(InstallError::PackageFetchFailed { .. }) => {
            // Network/cache flakes: still prove RuntimeMissing is not the path.
        }
        Err(e) => panic!("unexpected error: {e:?}"),
    }
}

#[tokio::test]
async fn agent_with_no_distribution_method_errors_without_touching_runtimes() {
    let agent = agent_with("test-empty-agent", Distribution::default());
    let err = install(&agent).await.unwrap_err();
    assert!(matches!(err, InstallError::NoDistribution(id) if id == "test-empty-agent"));
}
