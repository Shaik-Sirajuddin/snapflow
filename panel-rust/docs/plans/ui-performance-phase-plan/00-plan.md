# UI responsiveness and I/O offload plan

## Scope

Remove avoidable GUI-thread stalls while typing, streaming agent Markdown/tool
events, switching projects, saving/renaming projects, and resizing the Qt dock.
The plan covers both panel-rust and the Shotcut C++ host boundary.

## Baseline findings

- A changed transcript currently rebuilds the message projection on the poll/UI
  thread, including tool JSON classification and Markdown models.
- Agent rows compute both legacy `markdown-lines` and newer `markdown-blocks`.
- Tool-group reconciliation rewrites group rows even when their content is
  unchanged; the Slint chat mounts a flat row for every message.
- Agent event forwarding rebuilds merged transcripts and appends JSONL state
  synchronously for every message event.
- Qt geometry changes synchronously resize the software pixel buffer, resize
  every open PTY terminal, and request a full software repaint.
- Normal project switches are mostly asynchronous, but Save-As/rename performs
  SQLite/filesystem work inline during effect execution.
- The UI-fixes worktree has the Shotcut host as an uninitialized gitlink, so
  the Qt-side debounce must be implemented only after the host submodule is
  available; Rust-side resize work alone cannot coalesce calls that arrive
  synchronously from `geometryChange()`.

## Design decisions

1. Keep reducer/model state changes on the GUI thread, but move parsing,
   persistence, and filesystem work off it.
2. Preserve message ordering and stale-result protection with keyed identities
   and epochs; no background result may overwrite a newer stream or project.
3. Coalesce only visual stream updates. ACP event ordering and final TurnEnded,
   tool status, permission, and error events remain lossless.
4. Resize and project transitions are latest-value-wins operations with an
   explicit pending generation; intermediate geometry/path events may be
   skipped safely.

## Verification matrix

- Long Markdown stream with 100+ prior messages: typing remains responsive and
  no stale render replaces newer text.
- Tool/MCP stream with changing status/input/output: only affected rows update.
- Save/rename on a large project store: UI remains interactive and final path
  and store identity are correct after completion/failure.
- Dock resize drag with open terminals: no per-tick PTY/file-system blocking;
  final geometry and terminal dimensions converge.
- Project open/close/switch while a stream is active: old-project events do not
  leak into the new project.

## Execution evidence

- `tool_group_diff`: completed. Retained Slint models now skip unchanged nested
  tool rows and outer group slots while comparing stable group keys. Duplicate
  group keys receive deterministic position suffixes so retained VecModels
  cannot alias; duplicate message keys retain the first row index, and the row
  fingerprint covers all Slint-visible send/queue/first-use/grouping fields.
  Focused tests passed (9/9).
- `event_persistence_offload`: completed for ACP message-event appends. JSONL
  writes use `spawn_blocking` and remain sequential per forwarder/replay drain;
  serial-order regression passed. Synchronous `push_local` and runtime snapshot
  writes remain outside this phase because their synchronous APIs require a
  broader queue/flush design.
- `stream_delta_coalescing`: completed in the event forwarder. Ordered message
  history and persistence remain lossless, while transcript refreshes are
  emitted every four adjacent message events and flushed before semantic
  events or receiver shutdown.
- Archive cancellation: completed alongside the UI plan. Archiving a Loading
  thread immediately enters Cancelling and emits one native ACP cancel effect;
  archive spinner repaint excludes archived/closed threads. Focused tests passed.

The remaining stream coalescing, Markdown worker integration, chat windowing,
Qt/Rust resize coalescing, and Save-As offload phases are still pending and are
not marked complete in `meta.json`.
