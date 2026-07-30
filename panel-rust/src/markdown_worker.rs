//! Background render worker for Architecture v2 markdown blocks
//! (markdown-thread-freeze-fix phase 5). See the plan doc's "Background
//! render pipeline + chunked delivery" section:
//! `memory/acpx/gen/plans/panel-thread-switch-freeze-fix-plan.md`.
//!
//! ## Scope note
//!
//! This module implements and unit-tests the worker mechanism itself --
//! chunking, epoch-based cooperative cancellation, ack-gated
//! backpressure, `Send`-safety (spike-confirmed: `slint::StyledText` is
//! `Send`, `slint::ModelRc` is not, see `models::MarkdownBlockData`'s
//! doc comment) -- as a standalone, independently-testable piece. Wiring
//! it into the live `update.rs`/`dispatch.rs` message-list install path
//! (installing delivered chunks into the actually-displayed
//! `ModelRc<MessageItem>` and triggering a `Dirty::MessagesDiff`) is a
//! separate, larger integration deliberately left as a follow-up: phase
//! 1's row-cache work already found `update.rs`'s `switched_thread`/
//! `transcript_changed` control flow carries extensive existing
//! correctness comments (PISO-2, one-frame stale-collection guard) and
//! judged it too risky to restructure blind for that phase's narrower
//! fix. This module's `deliver`/`on_chunk` parameters are injected
//! specifically so that integration can be built and tested without
//! touching this file's already-tested chunking/cancellation logic.
//!
//! ## Delivery is injected, not hardcoded to `slint::invoke_from_event_loop`
//!
//! Production callers pass a `deliver` closure that wraps
//! `slint::invoke_from_event_loop` (this crate's `SpikeEventLoopProxy`
//! already queues such closures into `EVENT_LOOP_QUEUE`, drained every
//! tick by `panel_rust_poll` -- see `lib.rs`). Tests pass a synchronous
//! mock instead, so this module's logic is testable without depending on
//! Slint's platform-threading machinery being installed correctly in
//! the test process.

use crate::models::{build_markdown_block_data, MarkdownBlockData};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};

/// Render generation counter, injected rather than a module-global
/// static. A real app has exactly one logical "what's the current
/// selected thread's render generation" value -- genuinely global in
/// spirit -- but a module-global `static` sharing that value across
/// every `#[test]` in this same test binary (Rust runs them in parallel
/// by default) caused real cross-test interference: one test's
/// `bump_epoch()` could invalidate a concurrently-running test's epoch
/// before its worker ever produced a chunk. `EpochCounter` fixes this
/// by being a cheap-to-clone handle a caller owns: production code
/// constructs exactly one (e.g. held on `Model`/`PanelSingleton`) and
/// reuses it across every switch; each test constructs its own,
/// isolated by construction rather than by test-ordering discipline.
#[derive(Clone, Default, Debug)]
pub struct EpochCounter(Arc<AtomicU64>);

impl EpochCounter {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU64::new(0)))
    }

    /// Starts a new render generation, invalidating any prior in-flight
    /// worker holding an older epoch (its next check against
    /// [`Self::current`] observes this new value and stops producing
    /// further chunks, not just having delivered ones dropped). Returns
    /// the new epoch to pass to [`spawn_background_render`].
    pub fn bump(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// The render generation currently considered "wanted" -- whatever
    /// the most recent [`Self::bump`] call on this same counter returned.
    pub fn current(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Concurrency test matrix row 4 ("rapid switch back to same thread
/// mid-render"): tracks which `(thread_id, epoch)` renders are currently
/// in flight so a caller that spawns a second render for the *exact
/// same* switch (same thread, same epoch -- e.g. a rapid switch-away-
/// then-back where the epoch counter hasn't moved between the two
/// dispatches) doesn't pay for a fully redundant duplicate worker.
///
/// Deliberately scoped to `(thread_id, epoch)`, not `thread_id` alone: a
/// genuine switch back to a thread bumps the epoch via
/// [`EpochCounter::bump`], which is a legitimately new render request,
/// not a duplicate -- the old (now-superseded) epoch's worker is left to
/// notice via its own cooperative cancellation check, same as any other
/// stale-epoch case. This module does not implement full "rejoin an
/// in-flight render and multiplex delivery to multiple subscribers"
/// semantics (a caller whose spawn call was de-duped gets no `on_chunk`/
/// `on_done` of its own) -- the already-in-flight worker for that exact
/// `(thread_id, epoch)` is assumed to be the one satisfying the original
/// caller who started it.
#[derive(Clone, Default, Debug)]
pub struct InFlightRegistry(Arc<Mutex<HashSet<(String, u64)>>>);

impl InFlightRegistry {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(HashSet::new())))
    }

    /// Marks `(thread_id, epoch)` as in flight if it wasn't already.
    /// Returns `true` if this call actually started it (caller should
    /// proceed to spawn); `false` if a render for this exact
    /// `(thread_id, epoch)` was already running (caller should skip --
    /// spawning would be a fully redundant duplicate).
    fn try_start(&self, thread_id: &str, epoch: u64) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((thread_id.to_string(), epoch))
    }

    fn finish(&self, thread_id: &str, epoch: u64) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(thread_id.to_string(), epoch));
    }
}

/// One delivered chunk: up to [`CHUNK_MESSAGES_PER_STEP`] messages'
/// worth of already-styled blocks, in order, tagged by durable message
/// key and a content-hash fingerprint of the source text that produced
/// them -- markdown-render-cache-layer plan Phase 2. Deliberately not a
/// positional index into the original `messages` list: that list's
/// positions are only valid at spawn time, and by the time a chunk is
/// delivered (this worker runs concurrently with the UI thread) the
/// thread's actual row positions may have shifted from inserts/removes
/// elsewhere -- `key` is what a receiver must resolve against the
/// *current* `ThreadMessageIndex`, and `source_hash` is what proves the
/// resolved row's content hasn't changed since this render was
/// requested (see `thread_message_index::hash_content`'s doc comment
/// for why worker and index must share one hash implementation).
pub struct MessageBlocksChunk {
    pub thread_id: String,
    pub epoch: u64,
    /// `(key, source_hash, blocks)` triples, in the same order as this
    /// chunk's slice of `messages`.
    pub messages: Vec<(String, u64, Vec<MarkdownBlockData>)>,
}

/// How many messages' worth of blocks one worker step parses before
/// handing a chunk back to the UI thread and waiting for the ack gate.
/// Matches the plan doc's "~20-50 blocks at a time" chunk-size
/// discussion, sized in messages here since block-count-per-message
/// varies (a single-paragraph reply vs. a long structured response).
const CHUNK_MESSAGES_PER_STEP: usize = 20;

/// Spawns a background worker rendering `messages` (message texts, in
/// order, already filtered to whichever are markdown-eligible by the
/// caller -- this module doesn't know about message `kind`) for
/// `thread_id` at `epoch`.
///
/// Delivers one [`MessageBlocksChunk`] per up-to-[`CHUNK_MESSAGES_PER_
/// STEP`] messages via `deliver`, which is expected to eventually invoke
/// `on_chunk` on the UI thread (production: through `slint::
/// invoke_from_event_loop`; tests: synchronously). **Ack-gated**: the
/// worker blocks after handing a chunk to `deliver` until `on_chunk`
/// signals completion (by calling the `ack` closure passed to it) --
/// see the plan doc's "Backpressure" section for why this exists: an
/// unconditional push-as-fast-as-parsed flood could queue many small
/// UI-thread callbacks back-to-back and itself become a stutter.
///
/// **Cooperative cancellation**: before starting each chunk, the worker
/// checks `epoch_counter.current() == epoch` and returns early if a
/// newer switch has superseded this render -- an abandoned render stops
/// doing further work, it doesn't just have its output ignored on arrival
/// (test matrix case 3 in the plan doc).
///
/// `on_done(thread_id, epoch)` fires once after the last chunk (or
/// immediately, with an empty `messages` list) via the same `deliver`
/// mechanism, so callers can clear a `loading` flag.
///
/// `in_flight` de-dupes against a render already running for this exact
/// `(thread_id, epoch)` (test matrix row 4) -- if one is already in
/// flight, this call is a no-op: it returns immediately without spawning
/// a thread or calling `on_chunk`/`on_done` at all (the already-running
/// worker for that same request is assumed to satisfy the original
/// caller). The returned `JoinHandle` still always resolves quickly in
/// that case (a trivial no-op thread), so callers that unconditionally
/// `.join()` the result don't need to special-case the de-duped path.
pub fn spawn_background_render(
    thread_id: String,
    epoch: u64,
    epoch_counter: EpochCounter,
    in_flight: InFlightRegistry,
    // `(key, text)` pairs -- `key` is the durable message key
    // (`models::transcript_row_key`'s format) a receiver resolves
    // against `ThreadMessageIndex`, not a positional index (see
    // `MessageBlocksChunk`'s doc comment for why).
    messages: Vec<(String, String)>,
    deliver: impl Fn(Box<dyn FnOnce() + Send>) + Send + Sync + 'static,
    on_chunk: impl Fn(MessageBlocksChunk, Box<dyn FnOnce() + Send>) + Send + Sync + 'static,
    on_done: impl FnOnce(String, u64) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    if !in_flight.try_start(&thread_id, epoch) {
        return std::thread::spawn(|| {});
    }
    let finish_thread_id = thread_id.clone();
    let on_done = move |thread_id: String, epoch: u64| {
        in_flight.finish(&finish_thread_id, epoch);
        on_done(thread_id, epoch);
    };
    let job = render_job(thread_id, epoch, epoch_counter, messages, deliver, on_chunk, on_done);
    std::thread::spawn(job)
}

/// Same as [`spawn_background_render`], but submits the render job to a
/// [`RenderWorkerPool`] instead of paying `std::thread::spawn`'s OS
/// thread-creation cost on every single call -- see the plan doc's test
/// matrix row 5 ("thread reuse instead of spawning each time to delay
/// work"). Fire-and-forget: unlike the raw-spawn version there's no
/// `JoinHandle` to return (the pool's own threads persist past any one
/// job), so callers observe completion via `on_done`/the delivered
/// chunks instead of joining. Same `in_flight` de-dupe semantics as
/// [`spawn_background_render`].
pub fn spawn_background_render_pooled(
    pool: &RenderWorkerPool,
    thread_id: String,
    epoch: u64,
    epoch_counter: EpochCounter,
    in_flight: InFlightRegistry,
    // See spawn_background_render's `messages` comment above.
    messages: Vec<(String, String)>,
    deliver: impl Fn(Box<dyn FnOnce() + Send>) + Send + Sync + 'static,
    on_chunk: impl Fn(MessageBlocksChunk, Box<dyn FnOnce() + Send>) + Send + Sync + 'static,
    on_done: impl FnOnce(String, u64) + Send + 'static,
) {
    if !in_flight.try_start(&thread_id, epoch) {
        return;
    }
    let finish_thread_id = thread_id.clone();
    let on_done = move |thread_id: String, epoch: u64| {
        in_flight.finish(&finish_thread_id, epoch);
        on_done(thread_id, epoch);
    };
    let job = render_job(thread_id, epoch, epoch_counter, messages, deliver, on_chunk, on_done);
    pool.submit(job);
}

/// The actual chunking/cancellation/backpressure loop, factored out so
/// both [`spawn_background_render`] (raw `std::thread::spawn`, one OS
/// thread per call) and [`spawn_background_render_pooled`] (a shared,
/// persistent [`RenderWorkerPool`]) run identical logic -- only *how*
/// the resulting closure gets a thread to run on differs.
fn render_job(
    thread_id: String,
    epoch: u64,
    epoch_counter: EpochCounter,
    messages: Vec<(String, String)>,
    deliver: impl Fn(Box<dyn FnOnce() + Send>) + Send + Sync + 'static,
    on_chunk: impl Fn(MessageBlocksChunk, Box<dyn FnOnce() + Send>) + Send + Sync + 'static,
    on_done: impl FnOnce(String, u64) + Send + 'static,
) -> impl FnOnce() + Send + 'static {
    let deliver = std::sync::Arc::new(deliver);
    let on_chunk = std::sync::Arc::new(on_chunk);
    move || {
        let mut on_done = Some(on_done);
        let mut index = 0usize;
        while index < messages.len() {
            if epoch_counter.current() != epoch {
                return;
            }
            // markdown-render-cache-layer MCP-06 coverage: real parsing
            // (pulldown-cmark over a chat-sized message) finishes in
            // microseconds, far too fast for a live e2e test to reliably
            // land a thread-switch/new-message epoch bump *between* two
            // chunk iterations of this loop -- the epoch-drop check above
            // is already unit-tested with a synchronous `sync_deliver`,
            // but nothing exercises it against a real, independently
            // timed interrupt. `RUI_MARKDOWN_RENDER_TEST_DELAY_MS`
            // (unset in production; every real caller leaves it unset)
            // lets a live test widen that window deterministically
            // instead of racing real parse speed. Checked once per outer
            // loop iteration (message-chunk granularity), not per
            // message, so it delays between deliveries the same way a
            // slow real render would.
            if let Ok(ms) = std::env::var("RUI_MARKDOWN_RENDER_TEST_DELAY_MS") {
                if let Ok(ms) = ms.parse::<u64>() {
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                }
            }
            let end = (index + CHUNK_MESSAGES_PER_STEP).min(messages.len());
            // Test matrix case 6, "worker panics mid-parse": a malformed
            // or pathological message could in principle panic somewhere
            // inside pulldown-cmark/StyledText parsing. Without this,
            // that panic would just unwind the worker thread silently --
            // `on_done` never fires, so a UI-side `loading` flag this is
            // wired to (once the deferred update.rs/dispatch.rs
            // integration lands) would stay stuck forever with no
            // indication anything went wrong. Catch it here, stop the
            // render, and still fire `on_done` through the normal
            // `deliver` mechanism so the caller's cleanup path runs the
            // same way it would on a clean finish.
            let chunk_messages = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                messages[index..end]
                    .iter()
                    .map(|(key, text)| {
                        (
                            key.clone(),
                            crate::thread_message_index::hash_content(text),
                            build_markdown_block_data(text, false),
                        )
                    })
                    .collect::<Vec<(String, u64, Vec<MarkdownBlockData>)>>()
            }));
            let chunk_messages = match chunk_messages {
                Ok(chunk_messages) => chunk_messages,
                Err(_panic) => {
                    if let Some(on_done) = on_done.take() {
                        let done_closure = Box::new(move || {
                            on_done(thread_id, epoch);
                        }) as Box<dyn FnOnce() + Send>;
                        deliver(done_closure);
                    }
                    return;
                }
            };
            let chunk = MessageBlocksChunk {
                thread_id: thread_id.clone(),
                epoch,
                messages: chunk_messages,
            };

            // Ack gate: buffered by 1, not a capacity-0 rendezvous --
            // rendezvous send() blocks until a receiver is *already*
            // waiting, which self-deadlocks whenever `deliver` runs
            // synchronously on this same thread (`ack.send()` would run
            // before this thread ever reaches `ack_rx.recv()` below, so
            // there's nothing to rendezvous with yet). Capacity 1 lets
            // `send()` always complete immediately (only one ack is ever
            // sent per chunk), while `recv()` below still genuinely
            // blocks this thread until that ack has actually been sent --
            // the real backpressure guarantee this exists for.
            let (ack_tx, ack_rx) = sync_channel::<()>(1);
            let on_chunk_for_delivery = on_chunk.clone();
            let ack = Box::new(move || {
                let _ = ack_tx.send(());
            }) as Box<dyn FnOnce() + Send>;
            let deliver_closure = Box::new(move || {
                on_chunk_for_delivery(chunk, ack);
            }) as Box<dyn FnOnce() + Send>;
            deliver(deliver_closure);
            // Block until the UI thread has actually drained this
            // chunk before parsing the next one -- paces this worker to
            // the UI thread's real drain rate rather than this thread's
            // raw parse speed.
            //
            // `recv()` returning `Err` means `ack_tx` was dropped
            // without ever sending -- the delivery closure was dropped
            // instead of run (e.g. `deliver`'s target event loop already
            // shut down; test matrix case 8, "app/window closed while
            // worker active"). Stop here rather than silently proceeding
            // to the next chunk: continuing would burn CPU parsing
            // output nobody will ever see, and the original version of
            // this code swallowed the error (`let _ = ack_rx.recv();`),
            // which rust-audit's "silently swallowed Results on
            // anything connection-/lock-related" rule flags as exactly
            // this class of bug -- caught in review, not by a test.
            if ack_rx.recv().is_err() {
                return;
            }

            index = end;
        }

        if let Some(on_done) = on_done.take() {
            let done_closure = Box::new(move || {
                on_done(thread_id, epoch);
            }) as Box<dyn FnOnce() + Send>;
            deliver(done_closure);
        }
    }
}

/// A small, fixed-size pool of persistent OS threads pulling render jobs
/// from a shared queue, instead of `std::thread::spawn` paying OS
/// thread-creation cost on every single thread-switch (plan doc test
/// matrix row 5). Threads run until the pool itself is dropped (closing
/// the job channel, which ends each worker thread's receive loop).
pub struct RenderWorkerPool {
    sender: std::sync::mpsc::Sender<Box<dyn FnOnce() + Send>>,
    _threads: Vec<std::thread::JoinHandle<()>>,
}

impl RenderWorkerPool {
    pub fn new(size: usize) -> Self {
        let (sender, receiver) = std::sync::mpsc::channel::<Box<dyn FnOnce() + Send>>();
        let receiver = Arc::new(std::sync::Mutex::new(receiver));
        let threads = (0..size)
            .map(|_| {
                let receiver = receiver.clone();
                std::thread::spawn(move || loop {
                    let job = {
                        // Lock held only long enough to pull one job off
                        // the queue, never across running it -- matches
                        // the "never hold a lock across the actual work"
                        // rule (rust-audit / the slint skill's poll-thread
                        // anti-pattern section), even though this isn't
                        // the UI poll thread itself.
                        let receiver = receiver.lock().unwrap_or_else(|e| e.into_inner());
                        receiver.recv()
                    };
                    match job {
                        Ok(job) => job(),
                        Err(_) => break, // pool dropped, channel closed
                    }
                })
            })
            .collect();
        Self { sender, _threads: threads }
    }

    pub fn submit(&self, job: impl FnOnce() + Send + 'static) {
        let _ = self.sender.send(Box::new(job));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// This module's own doc comment on `EpochCounter` explains why a
    /// module-global `static` caused real cross-test interference once
    /// already (parallel `#[test]` runs). `RUI_MARKDOWN_RENDER_TEST_
    /// DELAY_MS` (the MCP-06 live-e2e hook, see `render_job`) is a
    /// process-wide env var, not an injectable instance, so it hits the
    /// same problem from the other direction: one test setting it would
    /// otherwise silently slow down any other test's concurrently-
    /// running `render_job` call. Every test below takes this lock
    /// first so at most one runs at a time, making the env var safe to
    /// mutate from exactly one of them.
    static TEST_SERIAL_LOCK: Mutex<()> = Mutex::new(());

    /// Synchronous test `deliver`: instead of queuing for a later poll
    /// tick, runs the closure immediately on whichever thread calls
    /// `deliver` (the worker thread, in these tests) -- fine for testing
    /// this module's own chunking/cancellation/backpressure logic, which
    /// doesn't care which thread `on_chunk` actually executes on.
    fn sync_deliver() -> impl Fn(Box<dyn FnOnce() + Send>) + Send + Sync + 'static {
        |f| f()
    }

    #[test]
    fn render_test_delay_env_var_lets_a_mid_render_epoch_bump_be_observed_deterministically() {
        let _serial = TEST_SERIAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Proves the MCP-06 live-e2e hook actually does what it claims:
        // widening the per-iteration window with
        // `RUI_MARKDOWN_RENDER_TEST_DELAY_MS` lets an epoch bump that
        // arrives *after* the first chunk was already handed to `deliver`
        // but *before* the worker starts its next iteration reliably land
        // in that gap -- without it, this same interleaving would be a
        // real-parse-speed race, not a deterministic repro. A live host
        // test widens the same window to fit a real thread-switch instead
        // of a `std::thread::sleep`.
        struct EnvGuard;
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                std::env::remove_var("RUI_MARKDOWN_RENDER_TEST_DELAY_MS");
            }
        }
        std::env::set_var("RUI_MARKDOWN_RENDER_TEST_DELAY_MS", "80");
        let _guard = EnvGuard;

        // 3 chunk-iterations' worth (CHUNK_MESSAGES_PER_STEP == 20).
        let messages: Vec<(String, String)> = (0..45)
            .map(|i| (format!("k{i}"), format!("message {i}")))
            .collect();
        let epoch_counter = EpochCounter::new();
        let epoch = epoch_counter.bump();
        let delivered_chunks = Arc::new(Mutex::new(0usize));
        let done = Arc::new(Mutex::new(false));

        let dc = delivered_chunks.clone();
        let done_flag = done.clone();
        let handle = spawn_background_render(
            "thread-a".into(),
            epoch,
            epoch_counter.clone(),
            InFlightRegistry::new(),
            messages,
            sync_deliver(),
            move |_chunk, ack| {
                *dc.lock().unwrap() += 1;
                ack();
            },
            move |_thread_id, _epoch| {
                *done_flag.lock().unwrap() = true;
            },
        );

        // Shorter than the 80ms per-iteration delay above, so this lands
        // while the worker is asleep inside its first iteration -- by the
        // time it wakes and reaches the second iteration's epoch check,
        // the bump below has already made this epoch stale.
        std::thread::sleep(std::time::Duration::from_millis(20));
        epoch_counter.bump();

        handle.join().expect("worker thread must not panic");

        let chunks_delivered = *delivered_chunks.lock().unwrap();
        assert!(
            chunks_delivered >= 1,
            "the first chunk's epoch check ran before the bump, so it must still be delivered"
        );
        assert!(
            chunks_delivered < 3,
            "the render must stop before delivering every chunk once its epoch goes stale mid-flight"
        );
        assert!(
            !*done.lock().unwrap(),
            "on_done must not fire for a render interrupted mid-flight, only for a clean finish"
        );
    }

    #[test]
    fn pooled_render_delivers_all_messages_same_as_raw_spawn() {
        let _serial = TEST_SERIAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Functional parity check: render_job's behavior must be
        // identical regardless of which mechanism (raw std::thread::spawn
        // vs a shared RenderWorkerPool) actually runs it.
        let pool = RenderWorkerPool::new(2);
        let messages: Vec<(String, String)> = (0..45)
            .map(|i| (format!("k{i}"), format!("message {i}")))
            .collect();
        let epoch_counter = EpochCounter::new();
        let epoch = epoch_counter.bump();
        let delivered_keys: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(Mutex::new(false));

        let dk = delivered_keys.clone();
        let done_flag = done.clone();
        spawn_background_render_pooled(
            &pool,
            "thread-a".into(),
            epoch,
            epoch_counter,
            InFlightRegistry::new(),
            messages,
            sync_deliver(),
            move |chunk, ack| {
                for (key, _hash, _blocks) in &chunk.messages {
                    dk.lock().unwrap().push(key.clone());
                }
                ack();
            },
            move |_thread_id, _epoch| {
                *done_flag.lock().unwrap() = true;
            },
        );

        // No JoinHandle from the pooled variant (fire-and-forget, matching
        // production usage) -- poll for completion instead, bounded so a
        // real hang still fails the test rather than blocking forever.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !*done.lock().unwrap() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        assert!(*done.lock().unwrap(), "pooled render must complete within the deadline");
        let keys = delivered_keys.lock().unwrap();
        let expected: Vec<String> = (0..45).map(|i| format!("k{i}")).collect();
        assert_eq!(*keys, expected, "pooled dispatch delivers every message key exactly once, in order");
    }

    #[test]
    fn pool_handles_many_rapid_submissions_without_dropping_or_hanging() {
        let _serial = TEST_SERIAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Test matrix row 5's core functional claim: a bounded pool can
        // absorb many rapid consecutive thread-switch renders (the
        // scenario a `std::thread::spawn`-per-switch design pays OS
        // thread-creation cost for on every single one) without losing
        // work or deadlocking.
        let pool = RenderWorkerPool::new(4);
        const SWITCHES: usize = 50;
        let completed = Arc::new(Mutex::new(0usize));

        for i in 0..SWITCHES {
            let epoch_counter = EpochCounter::new();
            let epoch = epoch_counter.bump();
            let done = completed.clone();
            spawn_background_render_pooled(
                &pool,
                format!("thread-{i}"),
                epoch,
                epoch_counter,
                InFlightRegistry::new(),
                vec![(format!("k{i}"), format!("switch {i} message"))],
                sync_deliver(),
                |_chunk, ack| ack(),
                move |_thread_id, _epoch| {
                    *done.lock().unwrap() += 1;
                },
            );
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while *completed.lock().unwrap() < SWITCHES && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        assert_eq!(*completed.lock().unwrap(), SWITCHES, "every rapid submission completes, none dropped or hung");
    }

    #[test]
    fn pooled_dispatch_has_no_spawn_cost_tail_vs_raw_spawn_per_switch() {
        let _serial = TEST_SERIAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Best-effort timing comparison (loose bound, not a strict
        // pass/fail on exact numbers, to avoid flakiness from OS thread
        // scheduling variance): time-to-first-chunk across many rapid
        // switches, raw std::thread::spawn-per-switch vs a shared pool.
        // Asserts the pooled p95 isn't *worse* than raw's median by more
        // than a generous margin -- pooling should never make this worse,
        // even if the exact speedup varies by machine/load.
        const SAMPLES: usize = 30;

        let mut raw_latencies = Vec::with_capacity(SAMPLES);
        for i in 0..SAMPLES {
            let epoch_counter = EpochCounter::new();
            let epoch = epoch_counter.bump();
            let start = std::time::Instant::now();
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            let handle = spawn_background_render(
                format!("raw-{i}"),
                epoch,
                epoch_counter,
                InFlightRegistry::new(),
                vec![("k".to_string(), "hello world".to_string())],
                sync_deliver(),
                move |_chunk, ack| {
                    let _ = tx.send(());
                    ack();
                },
                |_, _| {},
            );
            rx.recv_timeout(std::time::Duration::from_secs(5)).expect("raw chunk delivered");
            raw_latencies.push(start.elapsed());
            handle.join().unwrap();
        }

        let pool = RenderWorkerPool::new(4);
        let mut pooled_latencies = Vec::with_capacity(SAMPLES);
        for i in 0..SAMPLES {
            let epoch_counter = EpochCounter::new();
            let epoch = epoch_counter.bump();
            let start = std::time::Instant::now();
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            spawn_background_render_pooled(
                &pool,
                format!("pooled-{i}"),
                epoch,
                epoch_counter,
                InFlightRegistry::new(),
                vec![("k".to_string(), "hello world".to_string())],
                sync_deliver(),
                move |_chunk, ack| {
                    let _ = tx.send(());
                    ack();
                },
                |_, _| {},
            );
            rx.recv_timeout(std::time::Duration::from_secs(5)).expect("pooled chunk delivered");
            pooled_latencies.push(start.elapsed());
        }

        raw_latencies.sort();
        pooled_latencies.sort();
        let raw_median = raw_latencies[SAMPLES / 2];
        let pooled_p95 = pooled_latencies[(SAMPLES * 95 / 100).min(SAMPLES - 1)];

        // Generous margin (10x raw's median) -- the point isn't to prove
        // an exact speedup number (machine/load-dependent, would be
        // flaky), it's to catch a pool that's pathologically *worse* than
        // raw spawn-per-switch, which would mean the pool itself is
        // broken (e.g. contended lock serializing all jobs onto one
        // thread) rather than just "not obviously faster."
        assert!(
            pooled_p95 <= raw_median * 10,
            "pooled p95 ({pooled_p95:?}) should not be pathologically worse than raw spawn-per-switch median ({raw_median:?})"
        );
    }

    #[test]
    fn delivers_all_messages_across_chunks_in_order() {
        let _serial = TEST_SERIAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let messages: Vec<(String, String)> = (0..45)
            .map(|i| (format!("k{i}"), format!("message {i}")))
            .collect();
        let epoch_counter = EpochCounter::new();
        let epoch = epoch_counter.bump();
        let delivered_keys: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(Mutex::new(false));

        let dk = delivered_keys.clone();
        let done_flag = done.clone();
        let handle = spawn_background_render(
            "thread-a".into(),
            epoch,
            epoch_counter,
            InFlightRegistry::new(),
            messages,
            sync_deliver(),
            move |chunk, ack| {
                for (key, _hash, _blocks) in &chunk.messages {
                    dk.lock().unwrap().push(key.clone());
                }
                ack();
            },
            move |_thread_id, _epoch| {
                *done_flag.lock().unwrap() = true;
            },
        );
        handle.join().unwrap();

        let keys = delivered_keys.lock().unwrap();
        let expected: Vec<String> = (0..45).map(|i| format!("k{i}")).collect();
        assert_eq!(*keys, expected, "every message key delivered exactly once, in order");
        assert!(*done.lock().unwrap(), "on_done fired");
    }

    #[test]
    fn chunks_are_sized_at_most_chunk_messages_per_step() {
        let _serial = TEST_SERIAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let messages: Vec<(String, String)> = (0..45).map(|i| (format!("k{i}"), format!("message {i}"))).collect();
        let epoch_counter = EpochCounter::new();
        let epoch = epoch_counter.bump();
        let chunk_sizes: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));

        let sizes = chunk_sizes.clone();
        let handle = spawn_background_render(
            "thread-a".into(),
            epoch,
            epoch_counter,
            InFlightRegistry::new(),
            messages,
            sync_deliver(),
            move |chunk, ack| {
                sizes.lock().unwrap().push(chunk.messages.len());
                ack();
            },
            |_, _| {},
        );
        handle.join().unwrap();

        let sizes = chunk_sizes.lock().unwrap();
        assert_eq!(*sizes, vec![20, 20, 5], "45 messages -> 20+20+5, none over the cap");
    }

    #[test]
    fn stale_epoch_stops_the_worker_before_further_chunks_are_parsed() {
        let _serial = TEST_SERIAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Cooperative cancellation, not just "ignore stale output on
        // arrival": bump the epoch again from inside the first chunk's
        // callback (simulating a rapid switch-away mid-render), and
        // assert no further chunks ever get delivered.
        let messages: Vec<(String, String)> = (0..100).map(|i| (format!("k{i}"), format!("message {i}"))).collect();
        let epoch_counter = EpochCounter::new();
        let epoch = epoch_counter.bump();
        let chunk_count = Arc::new(Mutex::new(0usize));

        let count = chunk_count.clone();
        let counter_for_chunk = epoch_counter.clone();
        let handle = spawn_background_render(
            "thread-a".into(),
            epoch,
            epoch_counter,
            InFlightRegistry::new(),
            messages,
            sync_deliver(),
            move |_chunk, ack| {
                *count.lock().unwrap() += 1;
                // Simulate the user switching to a different thread
                // mid-render, right after the first chunk lands.
                counter_for_chunk.bump();
                ack();
            },
            |_, _| {},
        );
        handle.join().unwrap();

        // 100 messages / 20-per-chunk = 5 possible chunks; cancellation
        // after the first one must mean far fewer than 5 were produced.
        assert_eq!(*chunk_count.lock().unwrap(), 1, "worker stopped after the epoch it was spawned with was superseded");
    }

    #[test]
    fn different_epoch_at_spawn_time_never_delivers_any_chunk() {
        let _serial = TEST_SERIAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // A render spawned for an epoch that's already stale by the time
        // it starts (e.g. two rapid switches queued back to back) must
        // do zero work, not even one chunk.
        let messages: Vec<(String, String)> = (0..40).map(|i| (format!("k{i}"), format!("message {i}"))).collect();
        let epoch_counter = EpochCounter::new();
        let stale_epoch = epoch_counter.bump();
        epoch_counter.bump(); // supersede it before the worker ever starts
        let chunk_count = Arc::new(Mutex::new(0usize));

        let count = chunk_count.clone();
        let handle = spawn_background_render(
            "thread-a".into(),
            stale_epoch,
            epoch_counter,
            InFlightRegistry::new(),
            messages,
            sync_deliver(),
            move |_chunk, ack| {
                *count.lock().unwrap() += 1;
                ack();
            },
            |_, _| {},
        );
        handle.join().unwrap();

        assert_eq!(*chunk_count.lock().unwrap(), 0);
    }

    #[test]
    fn backpressure_worker_blocks_until_ack_before_producing_next_chunk() {
        let _serial = TEST_SERIAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Deliberately delay the ack for the first chunk and assert the
        // second chunk's callback hasn't run yet at that point -- proves
        // this is a real ack gate, not just a shape/count coincidence.
        let messages: Vec<(String, String)> = (0..40).map(|i| (format!("k{i}"), format!("message {i}"))).collect();
        let epoch_counter = EpochCounter::new();
        let epoch = epoch_counter.bump();
        let chunks_seen = Arc::new(Mutex::new(0usize));
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));

        let seen = chunks_seen.clone();
        let handle = spawn_background_render(
            "thread-a".into(),
            epoch,
            epoch_counter,
            InFlightRegistry::new(),
            messages,
            // Deliver on a background thread per chunk so the ack-wait
            // doesn't block the worker's own delivery call itself --
            // mirrors production, where `deliver` just enqueues and
            // returns immediately (the worker's blocking wait is on
            // `ack_rx.recv()`, not on `deliver` itself).
            |f| {
                std::thread::spawn(f);
            },
            move |chunk, ack| {
                // Drop the guard *before* the blocking recv below --
                // holding it across that block would deadlock the main
                // test thread's own `chunks_seen.lock()` in its assert,
                // which has to complete before the main thread ever
                // reaches the `release_tx.send()` that unblocks this.
                let is_first = {
                    let mut count = seen.lock().unwrap();
                    *count += 1;
                    *count == 1
                };
                if is_first {
                    // Block this callback (running on its own spawned
                    // thread per the `deliver` above) until the test
                    // explicitly releases it -- during that window the
                    // worker must still be waiting on `ack_rx.recv()`,
                    // proven by `chunks_seen` not reaching 2 below.
                    let _ = release_rx.lock().unwrap().recv();
                }
                let _ = chunk;
                ack();
            },
            |_, _| {},
        );

        // Give the first chunk's callback time to start and block.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(*chunks_seen.lock().unwrap(), 1, "second chunk must not start before the first is acked");

        release_tx.send(()).unwrap();
        handle.join().unwrap();
        assert_eq!(*chunks_seen.lock().unwrap(), 2, "second chunk proceeds once the first is acked");
    }

    #[test]
    fn duplicate_spawn_for_same_thread_and_epoch_is_a_no_op() {
        let _serial = TEST_SERIAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Matrix row 4: a second spawn_background_render call for the
        // *exact same* (thread_id, epoch) as an already-in-flight render
        // must not spawn a redundant duplicate worker -- its on_chunk/
        // on_done must never fire, and its returned JoinHandle must
        // still resolve promptly (a trivial no-op thread) so callers
        // that unconditionally .join() it don't hang.
        let epoch_counter = EpochCounter::new();
        let epoch = epoch_counter.bump();
        let in_flight = InFlightRegistry::new();
        // Deliberately <= CHUNK_MESSAGES_PER_STEP (20): this produces
        // exactly one chunk, so the single release_tx.send() below
        // unblocks it. With more than one chunk, on_chunk (which blocks
        // unconditionally, every call) would deadlock waiting for a
        // second release that never comes.
        let messages: Vec<(String, String)> = (0..5).map(|i| (format!("k{i}"), format!("message {i}"))).collect();

        // Keep the first render's single chunk callback blocked so its
        // (thread_id, epoch) entry stays registered as in-flight for the
        // whole test -- proves the *second* call's no-op-ness isn't just
        // a timing coincidence (first one finishing before the second
        // starts).
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let first_chunk_started = Arc::new(Mutex::new(false));
        let started_flag = first_chunk_started.clone();
        let first_handle = spawn_background_render(
            "thread-a".into(),
            epoch,
            epoch_counter.clone(),
            in_flight.clone(),
            messages,
            |f| {
                std::thread::spawn(f);
            },
            move |_chunk, ack| {
                *started_flag.lock().unwrap() = true;
                let _ = release_rx.lock().unwrap().recv();
                ack();
            },
            |_, _| {},
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !*first_chunk_started.lock().unwrap() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(*first_chunk_started.lock().unwrap(), "first render's chunk callback must have started");

        // Second call: identical thread_id + epoch + registry.
        let second_on_chunk_fired = Arc::new(Mutex::new(false));
        let second_flag = second_on_chunk_fired.clone();
        let second_on_done_fired = Arc::new(Mutex::new(false));
        let second_done_flag = second_on_done_fired.clone();
        let second_handle = spawn_background_render(
            "thread-a".into(),
            epoch,
            epoch_counter,
            in_flight,
            vec![("k".to_string(), "should never be parsed".to_string())],
            sync_deliver(),
            move |_chunk, _ack| {
                *second_flag.lock().unwrap() = true;
            },
            move |_thread_id, _epoch| {
                *second_done_flag.lock().unwrap() = true;
            },
        );
        second_handle.join().unwrap();

        assert!(!*second_on_chunk_fired.lock().unwrap(), "de-duped second spawn must never fire on_chunk");
        assert!(!*second_on_done_fired.lock().unwrap(), "de-duped second spawn must never fire on_done");

        // Unblock and confirm the *first*, real render still completes
        // normally -- de-dupe only skips the redundant second call, it
        // doesn't break the original.
        release_tx.send(()).unwrap();
        first_handle.join().unwrap();
    }

    #[test]
    fn different_epoch_for_same_thread_is_not_de_duped() {
        let _serial = TEST_SERIAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // A genuine switch back to a thread bumps the epoch -- that's a
        // legitimately new render request, not a duplicate, so it must
        // NOT be blocked by the in-flight registry even though the
        // thread_id is the same as a still-registered older epoch.
        let epoch_counter = EpochCounter::new();
        let in_flight = InFlightRegistry::new();
        let first_epoch = epoch_counter.bump();
        // Manually keep `first_epoch` "in flight" in the registry without
        // an actual running worker, to isolate exactly what's being
        // tested: does a *different* epoch for the same thread_id get
        // through the registry check.
        assert!(in_flight.try_start("thread-a", first_epoch));

        let second_epoch = epoch_counter.bump();
        let on_chunk_fired = Arc::new(Mutex::new(false));
        let fired = on_chunk_fired.clone();
        let handle = spawn_background_render(
            "thread-a".into(),
            second_epoch,
            epoch_counter,
            in_flight,
            vec![("k".to_string(), "hello".to_string())],
            sync_deliver(),
            move |_chunk, ack| {
                *fired.lock().unwrap() = true;
                ack();
            },
            |_, _| {},
        );
        handle.join().unwrap();

        assert!(*on_chunk_fired.lock().unwrap(), "a different epoch for the same thread_id must not be de-duped");
    }

    #[test]
    fn empty_message_list_still_calls_on_done() {
        let _serial = TEST_SERIAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let done = Arc::new(Mutex::new(false));
        let done_flag = done.clone();
        let epoch_counter = EpochCounter::new();
        let epoch = epoch_counter.bump();
        let handle = spawn_background_render(
            "thread-a".into(),
            epoch,
            epoch_counter,
            InFlightRegistry::new(),
            Vec::new(),
            sync_deliver(),
            |_chunk, _ack| panic!("on_chunk must not fire for an empty message list"),
            move |_thread_id, _epoch| {
                *done_flag.lock().unwrap() = true;
            },
        );
        handle.join().unwrap();
        assert!(*done.lock().unwrap());
    }

    #[test]
    fn dropped_delivery_stops_the_worker_instead_of_hanging_or_churning_silently() {
        let _serial = TEST_SERIAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Simulates a dead event loop (test matrix case 8): `deliver`
        // drops the closure instead of running it, so the ack channel's
        // sender is dropped without ever sending. The worker must stop
        // -- not hang forever on ack_rx.recv(), and not silently keep
        // parsing further chunks nobody will ever see.
        let messages: Vec<(String, String)> = (0..40).map(|i| (format!("k{i}"), format!("message {i}"))).collect();
        let epoch_counter = EpochCounter::new();
        let epoch = epoch_counter.bump();
        let on_chunk_calls = Arc::new(Mutex::new(0usize));
        let on_done_calls = Arc::new(Mutex::new(0usize));

        let calls = on_chunk_calls.clone();
        let done_calls = on_done_calls.clone();
        let handle = spawn_background_render(
            "thread-a".into(),
            epoch,
            epoch_counter,
            InFlightRegistry::new(),
            messages,
            |f| drop(f), // never runs the closure -- the "dead event loop"
            move |_chunk, _ack| {
                *calls.lock().unwrap() += 1;
            },
            move |_thread_id, _epoch| {
                *done_calls.lock().unwrap() += 1;
            },
        );
        // Must terminate (not hang) within a bounded time.
        handle.join().unwrap();

        assert_eq!(*on_chunk_calls.lock().unwrap(), 0, "on_chunk never runs since deliver drops the closure");
        assert_eq!(*on_done_calls.lock().unwrap(), 0, "worker stopped before reaching on_done, not after silently finishing");
    }

    #[test]
    fn blocks_are_real_markdown_block_data_not_placeholders() {
        let _serial = TEST_SERIAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let messages = vec![("k".to_string(), "# Heading\n\nBody text.".to_string())];
        let epoch_counter = EpochCounter::new();
        let epoch = epoch_counter.bump();
        let got_blocks: Arc<Mutex<Vec<MarkdownBlockData>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = got_blocks.clone();
        let handle = spawn_background_render(
            "thread-a".into(),
            epoch,
            epoch_counter,
            InFlightRegistry::new(),
            messages,
            sync_deliver(),
            move |chunk, ack| {
                for (_key, _hash, blocks) in chunk.messages {
                    sink.lock().unwrap().extend(blocks);
                }
                ack();
            },
            |_, _| {},
        );
        handle.join().unwrap();

        let blocks = got_blocks.lock().unwrap();
        assert_eq!(blocks.len(), 2, "one heading block + one paragraph block");
        assert_eq!(blocks[0].kind, "text");
        assert_eq!(blocks[0].default_font_size, 18.0, "h1 heading font size");
        assert_eq!(blocks[1].kind, "text");
        assert_eq!(blocks[1].default_font_size, 0.0, "body text inherits the view default");
    }

    #[test]
    fn delivered_source_hash_matches_thread_message_index_hash_content() {
        let _serial = TEST_SERIAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // markdown-render-cache-layer plan Phase 2's correctness glue: a
        // reducer applying a delivered chunk compares `source_hash`
        // against `ThreadMessageIndex::content_hash_for` (which is
        // `thread_message_index::hash_content` under the hood). If the
        // worker computed its `source_hash` with a different algorithm,
        // that comparison would never match even for byte-identical
        // text -- this proves both sides agree, not just that each side
        // is internally consistent with itself.
        let text = "Some **markdown** text.".to_string();
        let messages = vec![("k1".to_string(), text.clone())];
        let epoch_counter = EpochCounter::new();
        let epoch = epoch_counter.bump();
        let got_hash: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        let sink = got_hash.clone();
        let handle = spawn_background_render(
            "thread-a".into(),
            epoch,
            epoch_counter,
            InFlightRegistry::new(),
            messages,
            sync_deliver(),
            move |chunk, ack| {
                for (key, hash, _blocks) in &chunk.messages {
                    assert_eq!(key, "k1");
                    *sink.lock().unwrap() = Some(*hash);
                }
                ack();
            },
            |_, _| {},
        );
        handle.join().unwrap();

        let delivered_hash = got_hash.lock().unwrap().expect("chunk delivered");
        assert_eq!(delivered_hash, crate::thread_message_index::hash_content(&text));
    }
}
