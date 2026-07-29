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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::Arc;

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
#[derive(Clone, Default)]
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

/// One delivered chunk: up to [`CHUNK_MESSAGES_PER_STEP`] messages'
/// worth of already-styled blocks, in order, tagged with which messages
/// (by index into the original `messages` list passed to
/// [`spawn_background_render`]) they belong to.
pub struct MessageBlocksChunk {
    pub thread_id: String,
    pub epoch: u64,
    /// `(message_index, blocks)` pairs, in ascending `message_index`
    /// order, for this chunk's slice of `messages`.
    pub messages: Vec<(usize, Vec<MarkdownBlockData>)>,
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
pub fn spawn_background_render(
    thread_id: String,
    epoch: u64,
    epoch_counter: EpochCounter,
    messages: Vec<String>,
    deliver: impl Fn(Box<dyn FnOnce() + Send>) + Send + Sync + 'static,
    on_chunk: impl Fn(MessageBlocksChunk, Box<dyn FnOnce() + Send>) + Send + Sync + 'static,
    on_done: impl FnOnce(String, u64) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    let deliver = std::sync::Arc::new(deliver);
    let on_chunk = std::sync::Arc::new(on_chunk);
    std::thread::spawn(move || {
        let mut index = 0usize;
        while index < messages.len() {
            if epoch_counter.current() != epoch {
                return;
            }
            let end = (index + CHUNK_MESSAGES_PER_STEP).min(messages.len());
            let chunk_messages: Vec<(usize, Vec<MarkdownBlockData>)> = messages[index..end]
                .iter()
                .enumerate()
                .map(|(offset, text)| (index + offset, build_markdown_block_data(text, false)))
                .collect();
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

        let done_closure = Box::new(move || {
            on_done(thread_id, epoch);
        }) as Box<dyn FnOnce() + Send>;
        deliver(done_closure);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Synchronous test `deliver`: instead of queuing for a later poll
    /// tick, runs the closure immediately on whichever thread calls
    /// `deliver` (the worker thread, in these tests) -- fine for testing
    /// this module's own chunking/cancellation/backpressure logic, which
    /// doesn't care which thread `on_chunk` actually executes on.
    fn sync_deliver() -> impl Fn(Box<dyn FnOnce() + Send>) + Send + Sync + 'static {
        |f| f()
    }

    #[test]
    fn delivers_all_messages_across_chunks_in_order() {
        let messages: Vec<String> = (0..45).map(|i| format!("message {i}")).collect();
        let epoch_counter = EpochCounter::new();
        let epoch = epoch_counter.bump();
        let delivered_indices: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(Mutex::new(false));

        let di = delivered_indices.clone();
        let done_flag = done.clone();
        let handle = spawn_background_render(
            "thread-a".into(),
            epoch,
            epoch_counter,
            messages,
            sync_deliver(),
            move |chunk, ack| {
                for (idx, _blocks) in &chunk.messages {
                    di.lock().unwrap().push(*idx);
                }
                ack();
            },
            move |_thread_id, _epoch| {
                *done_flag.lock().unwrap() = true;
            },
        );
        handle.join().unwrap();

        let indices = delivered_indices.lock().unwrap();
        let expected: Vec<usize> = (0..45).collect();
        assert_eq!(*indices, expected, "every message index delivered exactly once, in order");
        assert!(*done.lock().unwrap(), "on_done fired");
    }

    #[test]
    fn chunks_are_sized_at_most_chunk_messages_per_step() {
        let messages: Vec<String> = (0..45).map(|i| format!("message {i}")).collect();
        let epoch_counter = EpochCounter::new();
        let epoch = epoch_counter.bump();
        let chunk_sizes: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));

        let sizes = chunk_sizes.clone();
        let handle = spawn_background_render(
            "thread-a".into(),
            epoch,
            epoch_counter,
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
        // Cooperative cancellation, not just "ignore stale output on
        // arrival": bump the epoch again from inside the first chunk's
        // callback (simulating a rapid switch-away mid-render), and
        // assert no further chunks ever get delivered.
        let messages: Vec<String> = (0..100).map(|i| format!("message {i}")).collect();
        let epoch_counter = EpochCounter::new();
        let epoch = epoch_counter.bump();
        let chunk_count = Arc::new(Mutex::new(0usize));

        let count = chunk_count.clone();
        let counter_for_chunk = epoch_counter.clone();
        let handle = spawn_background_render(
            "thread-a".into(),
            epoch,
            epoch_counter,
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
        // A render spawned for an epoch that's already stale by the time
        // it starts (e.g. two rapid switches queued back to back) must
        // do zero work, not even one chunk.
        let messages: Vec<String> = (0..40).map(|i| format!("message {i}")).collect();
        let epoch_counter = EpochCounter::new();
        let stale_epoch = epoch_counter.bump();
        epoch_counter.bump(); // supersede it before the worker ever starts
        let chunk_count = Arc::new(Mutex::new(0usize));

        let count = chunk_count.clone();
        let handle = spawn_background_render(
            "thread-a".into(),
            stale_epoch,
            epoch_counter,
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
        // Deliberately delay the ack for the first chunk and assert the
        // second chunk's callback hasn't run yet at that point -- proves
        // this is a real ack gate, not just a shape/count coincidence.
        let messages: Vec<String> = (0..40).map(|i| format!("message {i}")).collect();
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
    fn empty_message_list_still_calls_on_done() {
        let done = Arc::new(Mutex::new(false));
        let done_flag = done.clone();
        let epoch_counter = EpochCounter::new();
        let epoch = epoch_counter.bump();
        let handle = spawn_background_render(
            "thread-a".into(),
            epoch,
            epoch_counter,
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
        // Simulates a dead event loop (test matrix case 8): `deliver`
        // drops the closure instead of running it, so the ack channel's
        // sender is dropped without ever sending. The worker must stop
        // -- not hang forever on ack_rx.recv(), and not silently keep
        // parsing further chunks nobody will ever see.
        let messages: Vec<String> = (0..40).map(|i| format!("message {i}")).collect();
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
        let messages = vec!["# Heading\n\nBody text.".to_string()];
        let epoch_counter = EpochCounter::new();
        let epoch = epoch_counter.bump();
        let got_blocks: Arc<Mutex<Vec<MarkdownBlockData>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = got_blocks.clone();
        let handle = spawn_background_render(
            "thread-a".into(),
            epoch,
            epoch_counter,
            messages,
            sync_deliver(),
            move |chunk, ack| {
                for (_idx, blocks) in chunk.messages {
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
}
