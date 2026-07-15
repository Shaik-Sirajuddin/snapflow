#![cfg_attr(not(feature = "real_ffi"), allow(dead_code))]

//! Generic media-tooling helpers shared by every real `Backend` that needs
//! to shell out to `ffprobe`/`melt` -- originally lived inside
//! `mlt_backend.rs` (the now-removed standalone `MltBackend`), but these
//! particular functions have zero dependency on that backend's in-memory
//! project model: they are pure external-process wrappers (`ffprobe`
//! probing, `melt` binary resolution, codec-name normalization, stderr
//! scanning, job-map pruning) that `FfiBackend` (`ffi_backend.rs`) also
//! needs for its own `file.export`/`file.probe` implementations. Moved
//! here, unchanged, when `MltBackend` was deleted so this reuse could
//! continue without resurrecting the deleted backend.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

use crate::backend::{BackendError, BackendResult, FileProbe, JobStatus};

/// Project frame rate assumed throughout probing/duration-fallback math.
/// A real implementation would read this per-source; this fixes it
/// project-wide for simplicity (documented, not hidden).
pub(crate) const DEFAULT_FPS: i64 = 30;

/// Still-image codec names ffprobe reports for single-frame image files
/// (`png`, `mjpeg` for jpg, etc.) -- these never carry a `format.duration`
/// or `stream.duration` because a still image has no time dimension, so
/// `probe_media` falls back to `DEFAULT_IMAGE_DURATION_FRAMES` for these
/// instead of erroring. Real bug found via a live `claude -p` "photo
/// gallery" scenario run: `playlist.append`/`edit.appendClip` with
/// `source={"path": "*.png"}` unconditionally failed with "ffprobe
/// returned no duration" for *every* bare still image (confirmed via
/// direct `ffprobe` on both a freshly generated PNG and the pre-existing
/// `overlay.png` fixture -- neither ever reports a duration field), so no
/// still image could ever be used as a producer through this codepath.
pub(crate) const STILL_IMAGE_CODECS: &[&str] =
    &["png", "mjpeg", "bmp", "gif", "tiff", "webp", "targa", "ppm", "pgm", "pbm", "sgi"];

/// Default duration applied to a still image when ffprobe reports none,
/// matching real Shotcut's default still-image duration preference (4s)
/// rather than MLT's un-set producer default (10 minutes).
pub(crate) const DEFAULT_IMAGE_DURATION_FRAMES: i64 = DEFAULT_FPS * 4; // 4s

/// Cap on the number of `file.export` job records kept in memory at once.
/// `FfiBackend` keeps every export job (running or terminal) in an
/// `Arc<Mutex<HashMap<String, JobStatus>>>` for the entire lifetime of the
/// process -- and per `snapshotd/internal/sapproxy/router.go`'s connection
/// pooling, one backend process stays alive for one project *indefinitely*
/// (until the daemon or the project itself exits), not just for one RPC
/// session. Before `prune_finished_jobs` existed, nothing ever removed a
/// finished/errored/stopped job from this map, so an agent that calls
/// `file.export` repeatedly over a long project lifetime (retries,
/// iterative preview exports, ...) grew it without bound.
pub(crate) const MAX_TRACKED_JOBS: usize = 200;

pub(crate) fn normalize_vcodec(codec: &str) -> String {
    if codec.is_empty() {
        return "libx264".to_string();
    }
    match codec.to_ascii_lowercase().as_str() {
        "h264" | "avc" | "avc1" => "libx264".to_string(),
        "h265" | "hevc" => "libx265".to_string(),
        "vp8" => "libvpx".to_string(),
        "vp9" => "libvpx-vp9".to_string(),
        "av1" => "libaom-av1".to_string(),
        "prores" => "prores_ks".to_string(),
        _ => codec.to_string(),
    }
}

/// Defense-in-depth companion to `normalize_vcodec`: scans `melt`/libavcodec
/// stderr for the "<codec> unrecognised - ignoring" pattern that indicates a
/// stream was silently dropped despite a zero exit status, so `file_export`
/// can report the job as failed instead of "done" even for a codec value
/// this module doesn't already know how to normalize. Returns the offending
/// line if found.
pub(crate) fn detect_unrecognised_codec(stderr: &str) -> Option<&str> {
    stderr
        .lines()
        .find(|line| line.contains("unrecognised - ignoring") || line.contains("unrecognized - ignoring"))
}

/// Evicts terminal (non-`"running"`) jobs from `jobs` until its size is
/// back at or under `MAX_TRACKED_JOBS`, and returns the evicted ids so a
/// caller holding other per-job state keyed the same way (`job_children`)
/// can drop those entries too and stay in sync. A job still `"running"` is
/// never evicted -- even if that temporarily leaves `jobs` above the cap --
/// because its real outcome must stay retrievable via `jobs.get` until the
/// export actually finishes; only already-finished bookkeeping is bounded.
/// Which terminal jobs get evicted first when there are more than needed is
/// unspecified (arbitrary among terminal entries): `jobs.get` on a very old
/// finished job was never a guaranteed-forever contract, only "until we
/// needed the memory back".
pub(crate) fn prune_finished_jobs(jobs: &mut HashMap<String, JobStatus>) -> Vec<String> {
    if jobs.len() <= MAX_TRACKED_JOBS {
        return Vec::new();
    }
    let over = jobs.len() - MAX_TRACKED_JOBS;
    let evict: Vec<String> = jobs
        .iter()
        .filter(|(_, job)| job.status != "running")
        .map(|(id, _)| id.clone())
        .take(over)
        .collect();
    for id in &evict {
        jobs.remove(id);
    }
    evict
}

pub(crate) fn resolve_melt_binary() -> String {
    if let Ok(p) = std::env::var("MELT_BIN") {
        return p;
    }
    if let Ok(home) = std::env::var("HOME") {
        let candidate = format!("{home}/.local/bin/melt");
        if Path::new(&candidate).exists() {
            return candidate;
        }
    }
    "melt".to_string()
}

fn resolve_ffprobe_binary() -> String {
    std::env::var("FFPROBE_BIN").unwrap_or_else(|_| "ffprobe".to_string())
}

/// Real `ffprobe` invocation -- no guessed codecs or durations. Frame count
/// prefers `nb_frames` when the container reports it, else falls back to
/// `duration * DEFAULT_FPS` (accurate as long as the source's real frame
/// rate matches the project's fixed `DEFAULT_FPS`).
pub(crate) fn probe_media(path: &str) -> BackendResult<FileProbe> {
    let output = Command::new(resolve_ffprobe_binary())
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration:stream=codec_name,codec_type,nb_frames,duration",
            "-of",
            "json",
            path,
        ])
        .output()
        .map_err(|e| BackendError::InvalidParams(format!("failed to run ffprobe on {path}: {e}")))?;
    if !output.status.success() {
        return Err(BackendError::InvalidParams(format!(
            "ffprobe failed on {path}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let json: Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| BackendError::InvalidParams(format!("bad ffprobe JSON for {path}: {e}")))?;
    let streams = json
        .get("streams")
        .and_then(Value::as_array)
        .ok_or_else(|| BackendError::InvalidParams(format!("ffprobe returned no streams for {path}")))?;
    let stream = streams
        .iter()
        .find(|s| s.get("codec_type").and_then(Value::as_str) == Some("video"))
        .or_else(|| streams.first())
        .ok_or_else(|| BackendError::InvalidParams(format!("ffprobe returned no streams for {path}")))?;
    let codec = stream
        .get("codec_name")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError::InvalidParams(format!("ffprobe returned no codec for {path}")))?;
    let is_still_image = STILL_IMAGE_CODECS.contains(&codec);
    let probed_duration_seconds = json
        .get("format")
        .and_then(|f| f.get("duration"))
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| stream.get("duration").and_then(Value::as_str).and_then(|s| s.parse::<f64>().ok()));
    let duration_seconds = match probed_duration_seconds {
        Some(d) => d,
        None if is_still_image => DEFAULT_IMAGE_DURATION_FRAMES as f64 / DEFAULT_FPS as f64,
        None => return Err(BackendError::InvalidParams(format!("ffprobe returned no duration for {path}"))),
    };
    let duration_frames = stream
        .get("nb_frames")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|frames| *frames > 0)
        .unwrap_or_else(|| {
            if is_still_image {
                DEFAULT_IMAGE_DURATION_FRAMES
            } else {
                (duration_seconds * DEFAULT_FPS as f64).round() as i64
            }
        });
    if duration_frames <= 0 || !duration_seconds.is_finite() || duration_seconds < 0.0 {
        return Err(BackendError::InvalidParams(format!("ffprobe returned an invalid duration for {path}")));
    }
    Ok(FileProbe {
        path: path.to_string(),
        duration_seconds,
        duration_frames,
        codec: codec.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_finished_jobs_bounds_the_map_without_evicting_running_jobs() {
        let mut jobs: HashMap<String, JobStatus> = HashMap::new();
        for i in 0..(MAX_TRACKED_JOBS + 50) {
            jobs.insert(
                format!("done-{i}"),
                JobStatus {
                    job_id: format!("done-{i}"),
                    status: "done".into(),
                    percent: 100.0,
                    result_path: Some(format!("/tmp/out-{i}.mp4")),
                    error: None,
                },
            );
        }
        jobs.insert(
            "still-running".into(),
            JobStatus {
                job_id: "still-running".into(),
                status: "running".into(),
                percent: 40.0,
                result_path: Some("/tmp/out-running.mp4".into()),
                error: None,
            },
        );
        assert_eq!(jobs.len(), MAX_TRACKED_JOBS + 51);

        let evicted = prune_finished_jobs(&mut jobs);

        assert_eq!(jobs.len(), MAX_TRACKED_JOBS);
        assert_eq!(evicted.len(), 51);
        assert!(!evicted.contains(&"still-running".to_string()));
        assert!(jobs.contains_key("still-running"));
        for id in &evicted {
            assert!(!jobs.contains_key(id));
        }

        // A map already at/under the cap is left untouched.
        let mut small: HashMap<String, JobStatus> = HashMap::new();
        small.insert(
            "only-one".into(),
            JobStatus {
                job_id: "only-one".into(),
                status: "done".into(),
                percent: 100.0,
                result_path: None,
                error: None,
            },
        );
        assert!(prune_finished_jobs(&mut small).is_empty());
        assert_eq!(small.len(), 1);

        // Every job still running: nothing evictable, map stays over cap
        // rather than dropping a live job's status.
        let mut all_running: HashMap<String, JobStatus> = HashMap::new();
        for i in 0..(MAX_TRACKED_JOBS + 5) {
            all_running.insert(
                format!("run-{i}"),
                JobStatus {
                    job_id: format!("run-{i}"),
                    status: "running".into(),
                    percent: 0.0,
                    result_path: None,
                    error: None,
                },
            );
        }
        assert!(prune_finished_jobs(&mut all_running).is_empty());
        assert_eq!(all_running.len(), MAX_TRACKED_JOBS + 5);
    }

    #[test]
    fn normalize_vcodec_maps_common_aliases() {
        assert_eq!(normalize_vcodec("h264"), "libx264");
        assert_eq!(normalize_vcodec("hevc"), "libx265");
        assert_eq!(normalize_vcodec(""), "libx264");
        assert_eq!(normalize_vcodec("mjpeg"), "mjpeg");
    }

    #[test]
    fn detect_unrecognised_codec_finds_the_offending_line() {
        let stderr = "some noise\n[libx264 @ 0x1] frobnicate\nfoo unrecognised - ignoring\nmore noise";
        assert_eq!(detect_unrecognised_codec(stderr), Some("foo unrecognised - ignoring"));
        assert_eq!(detect_unrecognised_codec("all clean"), None);
    }
}
