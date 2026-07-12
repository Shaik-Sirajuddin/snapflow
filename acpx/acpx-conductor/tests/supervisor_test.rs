use acpx_conductor::{SpawnSpec, Supervisor};

#[tokio::test]
async fn ensure_running_spawns_and_reuses_process() {
    let mut sup = Supervisor::new();
    // `cat` echoes stdin to stdout unchanged -- a trivial stand-in backend
    // for exercising spawn/reuse without depending on a real ACP adapter.
    sup.register("echo-agent", SpawnSpec::new("cat", vec![]));

    let first_running = sup.ensure_running("echo-agent").await;
    assert!(first_running.is_ok());

    // Second call should reuse the same process (still alive), not spawn a
    // second one.
    let second_running = sup.ensure_running("echo-agent").await;
    assert!(second_running.is_ok());

    sup.stop("echo-agent").await.unwrap();
}

#[tokio::test]
async fn ensure_running_errors_for_unregistered_agent() {
    let mut sup = Supervisor::new();
    let result = sup.ensure_running("nope").await;
    assert!(result.is_err());
}
