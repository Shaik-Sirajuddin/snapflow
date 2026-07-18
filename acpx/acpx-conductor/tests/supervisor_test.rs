use acpx_conductor::{ProcessStatus, SpawnSpec, Supervisor, SupervisorError};
use std::time::Duration;

/// Regression test for the `STOP_LOCK_TIMEOUT` fix in
/// `Supervisor::stop` -- mirrors `acpx-core`'s
/// `stuck_backend_session_close_does_not_wedge_the_router` test for the
/// identical failure shape one layer down: a stuck in-flight request
/// holds the per-process lock `stop` used to wait on unboundedly, which
/// previously meant `stop` (and therefore any caller holding a broader
/// lock around it, e.g. `acpx-core::Router`'s global mutex) blocked
/// forever too.
#[tokio::test]
async fn stop_does_not_block_forever_when_process_lock_is_held() {
    let mut sup = Supervisor::new();
    sup.register("echo-agent", SpawnSpec::new("cat", vec![]));
    let handle = sup.ensure_running("echo-agent").await.unwrap();

    // Simulate a stuck in-flight request (e.g. `read_matching_response`
    // blocked reading a reply that never comes) by holding the process's
    // own lock indefinitely from another task.
    let held = handle.clone();
    let hold_task = tokio::spawn(async move {
        let _guard = held.lock().await;
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    // Give the holder task a moment to actually acquire the lock first.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let stop_result = tokio::time::timeout(Duration::from_secs(20), sup.stop("echo-agent")).await;
    assert!(
        stop_result.is_ok(),
        "stop() should return within STOP_LOCK_TIMEOUT instead of blocking forever \
         behind a stuck holder of the process lock"
    );
    stop_result.unwrap().unwrap();

    // `stop` must have detached bookkeeping immediately, not left the
    // agent looking like it is still running.
    assert_eq!(sup.status("echo-agent"), ProcessStatus::NotStarted);

    hold_task.abort();
}

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

#[tokio::test]
async fn status_reports_not_started_running_and_exited() {
    let mut sup = Supervisor::new();
    sup.register("echo-agent", SpawnSpec::new("cat", vec![]));

    // Never spawned yet.
    assert_eq!(sup.status("echo-agent"), ProcessStatus::NotStarted);

    sup.ensure_running("echo-agent").await.unwrap();
    assert_eq!(sup.status("echo-agent"), ProcessStatus::Running);

    sup.stop("echo-agent").await.unwrap();
    // Stop removes the process from tracking entirely, so it reports as
    // never-started again rather than exited.
    assert_eq!(sup.status("echo-agent"), ProcessStatus::NotStarted);
}

#[tokio::test]
async fn status_reports_exit_code_after_crash() {
    let mut sup = Supervisor::new();
    sup.register(
        "crash-agent",
        SpawnSpec::new("sh", vec!["-c".into(), "exit 7".into()]),
    );

    sup.ensure_running("crash-agent").await.unwrap();
    // Give the shell a moment to actually exit.
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        sup.status("crash-agent"),
        ProcessStatus::Exited { code: Some(7) }
    );
}

#[tokio::test]
async fn crash_triggers_backoff_before_respawn() {
    let mut sup = Supervisor::new();
    // A stand-in backend that crashes the moment it's spawned.
    sup.register(
        "crash-agent",
        SpawnSpec::new("sh", vec!["-c".into(), "exit 1".into()]),
    );

    let first = sup.ensure_running("crash-agent").await;
    assert!(first.is_ok(), "expected initial spawn to succeed");

    // Give the shell a brief moment to actually exit so the next call's
    // `has_exited()` check observes the crash.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The process has already crashed; calling ensure_running again should
    // be throttled by backoff rather than respawning instantly.
    let second = sup.ensure_running("crash-agent").await;
    match second {
        Err(SupervisorError::Backoff { retry_after, .. }) => {
            assert!(retry_after > Duration::ZERO);
            assert!(retry_after <= Duration::from_millis(500));
        }
        Ok(_) => panic!("expected Backoff error, got Ok"),
        Err(other) => panic!("expected Backoff error, got {other:?}"),
    }
}

#[tokio::test]
async fn stable_process_resets_backoff_counter_on_later_crash() {
    let mut sup = Supervisor::new();
    // Shorten the stability window so the test doesn't need to wait out the
    // real 10s default.
    sup.set_stable_after(Duration::from_millis(50));
    sup.register(
        "flaky-agent",
        SpawnSpec::new("sh", vec!["-c".into(), "sleep 0.2 && exit 1".into()]),
    );

    let first = sup.ensure_running("flaky-agent").await;
    assert!(first.is_ok(), "expected initial spawn to succeed");

    // Let it run well past the (shortened) stability threshold before it
    // crashes on its own.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The process survived past `stable_after` before crashing, so this
    // should be treated as an isolated crash -- not a backoff-worthy
    // failure -- and respawn immediately instead of erroring.
    let second = sup.ensure_running("flaky-agent").await;
    assert!(
        second.is_ok(),
        "expected immediate respawn after a stable run, got {:?}",
        second.err()
    );

    sup.stop("flaky-agent").await.unwrap();
}
