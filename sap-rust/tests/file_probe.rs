use std::process::Command;

use sap_rust::backend::Backend;
use sap_rust::mlt_backend::MltBackend;

fn generate_source(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("probe-source.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x90:rate=30:duration=2",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-loglevel",
            "error",
        ])
        .arg(&path)
        .status()
        .expect("failed to spawn ffmpeg");
    assert!(status.success(), "ffmpeg failed to generate probe source");
    path
}

#[test]
fn mlt_file_probe_returns_real_duration_and_codec() {
    if Command::new("ffprobe").arg("-version").output().is_err()
        || Command::new("ffmpeg").arg("-version").output().is_err()
    {
        eprintln!("skipping real file.probe test: ffmpeg/ffprobe unavailable");
        return;
    }

    let dir = std::env::temp_dir().join(format!("sap-rust-file-probe-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create probe temp dir");
    let source = generate_source(&dir);

    let mut backend = MltBackend::new(dir.join("projects"));
    let probe = backend.file_probe(source.to_str().expect("UTF-8 temp path")).expect("file.probe");

    assert_eq!(probe.path, source.to_string_lossy());
    assert_eq!(probe.duration_frames, 60);
    assert_eq!(probe.codec, "h264");
    assert!((probe.duration_seconds - 2.0).abs() < 0.05);
}
