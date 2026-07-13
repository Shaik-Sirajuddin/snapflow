# sap-rust

SAP (Snapshot App Protocol) JSON-RPC 2.0 server layer for the Shotcut fork, per
`memory/head/gen/rust-fork/{01-jsonrpc-spec,02-rust-embedding,05-multi-client-concurrency}.md`.
A sibling directory to `shotcut/`, not nested inside it — see `02-rust-embedding.md`'s
repo-layout section for why.

## What's real

- **Wire protocol** (`src/protocol.rs`): JSON-RPC 2.0 request/response/notification
  types, plus SAP's application error codes (`-32001..-32004`) layered on top of
  the standard reserved range.
- **Framing** (`src/framing.rs`): LSP-style `Content-Length` header framing over
  any `AsyncRead`/`AsyncWrite`, chosen (per `01`) to avoid newline-escaping issues
  in string params like file paths or notes text.
- **Multi-client dispatch and single-writer serialization** (`src/server.rs`):
  a `tokio::net::UnixListener` accepts any number of simultaneous connections.
  Every connection gets its own reader/writer task pair, but all of them funnel
  parsed, session-validated requests into **one shared dispatcher task** via an
  unbounded `mpsc` channel; that task is the sole owner of the `Backend` trait
  object and applies requests strictly FIFO, one at a time, across all
  connections. This is the in-process stand-in for `02`'s
  `QMetaObject::invokeMethod(..., Qt::BlockingQueuedConnection)` requirement —
  a real Qt-backed `Backend` can be dropped into `run_dispatcher` without
  touching any of the connection/session/notification plumbing.
- **Notification fan-out** (`src/server.rs`): mutating calls
  (`edit.addTrack`, `edit.removeTrack`, `edit.appendClip`, `notes.setText`,
  `project.save`/`undo`/`redo`) publish a notification (`edit.changed`,
  `notes.changed`, `project.dirty`) on a per-project `tokio::sync::broadcast`
  channel. Every connection currently bound to that project receives it —
  not just the connection that made the call — per `05`'s "comprehensive
  fan-out requirement".
- **Session-binding enforcement** (`src/server.rs`): every connection must send
  `sap.hello {token}` (checked against `ServerConfig::token`) before anything
  else is accepted, returning `UNAUTHENTICATED` (-32001) otherwise. After that,
  every `edit.*`/`playback.*`/`notes.*`/`project.save`/`project.undo`/
  `project.redo`/`project.getState` call requires a prior successful
  `project.select {projectId}`, returning `NO_PROJECT_BOUND` (-32002)
  otherwise. `sap.hello` with the wrong token returns `BAD_TOKEN` (-32003).
  Once bound, `edit.*`/`playback.*`/`notes.*` calls take no `projectId`
  parameter — the bound project is implicit session state, per `01`'s binding
  model — the server supplies it to the `Backend` call from session state, not
  from client-supplied params.

## What's mocked

- **The `Backend` trait implementation** (`src/backend.rs`): `MockBackend` is a
  plain in-memory stand-in for the real Qt/C-ABI FFI-backed implementation
  described in `02-rust-embedding.md` ("Option A: thin C-ABI shim + bindgen").
  It implements the exact same `Backend` trait a real implementation would, so
  swapping it out is a one-line change at the `serve()` call site in
  `src/main.rs` — nothing in `server.rs` (protocol, dispatch, session binding,
  fan-out) depends on `MockBackend` specifically.
- **`src/ffi.rs`**: inert `extern "C"` declarations behind the `real_ffi`
  Cargo feature (off by default). They describe the intended shape of bindings
  against a future `shotcut/src/rustbridge/sap_ffi.h` (per `02`'s example
  header) but are not linked into any build — there is no C library to link
  against in this sandbox. Enabling `real_ffi` alone does not make the crate
  call into Shotcut; it only exists so the declarations can be reviewed/reused
  once `sap_ffi.h`/`.cpp` exist in the fork.

## Method surface implemented

`sap.hello`, `project.select`, `project.exit`, `project.getState`,
`project.save`, `project.undo`, `project.redo`, `edit.addTrack`,
`edit.removeTrack`, `edit.listTracks`, `edit.appendClip`, `edit.listClips`,
`playback.seek`, `notes.getText`, `notes.setText` — a meaningful subset of
`01-jsonrpc-spec.md`'s full surface, matching what `Backend` (`src/backend.rs`)
already exposes. Namespaces not covered (`playlist.*`, `filter.*`,
`subtitles.*`, ...) follow the exact same routing pattern in `build_op` and are
a mechanical, not architectural, extension.

The doc-11-Phase-A surface below is now also implemented and routed (via
`build_op_ext` in `src/server.rs`): `playlist.append`, `playlist.list`,
`edit.trimClipIn`, `edit.trimClipOut`, `transitions.addCrossfade`,
`filter.add`, `filter.addKeyframe`, `generator.createTitle`,
`subtitles.addTrack`, `subtitles.appendItem`, `file.export`, `jobs.get`,
`playback.getFrame`. These are additive `Backend` trait methods — see
"Real FFI (shotcut integration)" below for `FfiBackend`, the one production
implementor of these (a since-removed second, Qt-free `MltBackend`
implementor used to also cover them for dev/CI purposes -- see git history
if that standalone-testable path is needed again).

## Build / test

Rust toolchain is installed via `rustup` but not on `PATH` by default:

```sh
source "$HOME/.cargo/env"   # once per shell
cd sap-rust
cargo build
cargo test
```

`real_ffi` build (still compiles nothing new, just gates `src/ffi.rs`'s
declarations into the build):

```sh
cargo build --features real_ffi
```

## `media_tools.rs` (generic ffprobe/melt helpers)

`FfiBackend`'s `file.export`/`file.probe` shell out directly to `melt`/
`ffprobe`, using real Shotcut's own `saveXML()`/`Controller::saveXML()` to
generate the MLT XML (not a hand-rolled builder -- see "Real FFI" below).
`src/media_tools.rs` holds the generic, backend-agnostic pieces of that:
`probe_media` (ffprobe wrapper), `resolve_melt_binary`/`normalize_vcodec`/
`detect_unrecognised_codec` (melt invocation + codec/stderr handling), and
`prune_finished_jobs` (bounding the in-memory export-job map).

These used to also back a second, Qt-free `Backend` implementor called
`MltBackend` (its own in-memory project model + hand-built MLT XML
generator, used only for `cargo test`-only coverage of doc 11's Phase A
scenario before a real Shotcut/Qt build was validated). That implementor
and its integration tests (`tests/mlt_export_integration.rs`,
`tests/doc11_phase_a_full.rs`, `tests/file_probe.rs`) were removed once
`FfiBackend` became the sole production backend -- see git history if
that standalone-testable path is needed again. Two real MLT/melt
behaviors it discovered empirically remain true and relevant to
`FfiBackend`'s own `melt`-based export:

1. **Multi-track video compositing** needs an explicit `qtblend`
   `<transition>` between tracks -- a bare `<tractor>` with multiple
   `<track>` elements and no compositing transition only shows the top
   track (confirmed by rendering a two-track probe with and without it).
   Real Shotcut's own `MultitrackModel::getVideoBlendTransition`/
   `addVideoTrack` already do this, so `FfiBackend` inherits it for free
   via `saveXML()`.
2. **MLT's legacy `rect`/`mlt_geometry`-typed properties** (e.g. `affine`'s
   `transition.rect`) tween back toward the *first* keyframe's value past
   the last explicit keyframe if nothing pins the end -- a 2-keyframe
   slide-in animation was observed sliding back out again with no third
   keyframe. A held end value needs an explicit keyframe at the last frame
   you want it to hold for. Numeric (non-rect) properties like
   `brightness`'s `level` do not have this quirk.
3. **Subtitle burn-in**: real Shotcut's own mechanism (`subtitle_feed`
   filter + `subtitle.N.feed`/`subtitle.N.lang` **consumer** properties)
   depends on a live Shotcut `Subtitles` QObject injecting per-frame cue
   text during rendering -- tested directly against `melt` with a real SRT
   file and it only produced an *empty* placeholder `mov_text` stream (0
   real packets) when driven as a bare CLI subprocess outside the GUI
   session. `avfilter.subtitles` (`av.filename=<srt path>`, attached as a
   post-composite filter) burns real, decodable text pixels in standalone,
   confirmed by decoding frames inside vs. outside a cue window.

Doc 11's Phase-A scenario (cut/arrange/crossfade/animate/title/subtitle/
export, chained in one project) needs to be re-proven against the real
`FfiBackend`/Qt build now (a live headless Shotcut process, not a plain
`cargo test`) -- see
`memory/exe/gen/plans/2026-07-13-plan-parity-alignment.md` and the
`scripts/*-parity-check.py` scripts for that in-progress work.

## Running standalone

```sh
cargo run -- --socket /tmp/sap-test.sock
```

In the real embedded deployment, the daemon sets `SNAPSHOT_SAP_SOCKET` and
`SNAPSHOT_SAP_TOKEN` before launching Snapshot (per `08-lifecycle-and-cli.md`);
`main.rs` prefers those env vars and only falls back to `--socket` when
`SNAPSHOT_SAP_SOCKET` is unset, so the same binary is runnable standalone for
manual testing.

## Real FFI (shotcut integration)

`real_ffi` (off by default) now wires an actual, non-mock path into a running
Shotcut fork process, per `02-rust-embedding.md`'s Option A and
`08-lifecycle-and-cli.md`'s startup sequence. This section is the current,
precise line between what's real and what's still stubbed for this pass.

### What's wired for real

- **The C-ABI shim** (`shotcut/src/rustbridge/sap_ffi.h`/`.cpp`, new files,
  the only new files inside `shotcut/src/`): thin `extern "C"` wrappers
  around the real `TimelineDock::addVideoTrack()`/`addAudioTrack()`/
  `removeTrack()`, `MultitrackModel::trackList()`, `MainWindow::saveXML()`
  (which calls `Controller::saveXML()`, mltcontroller.cpp:489), and
  `MainWindow::undoStack()`. Every function that touches Qt/MLT state
  crosses to the Qt main thread via
  `QMetaObject::invokeMethod(..., Qt::BlockingQueuedConnection)` before
  touching anything — the load-bearing rule from `02`, not optional.
- **`FfiBackend`** (`src/ffi_backend.rs`, new, behind `real_ffi`): a second
  implementor of the existing `Backend` trait (the trait itself is
  unchanged) that calls straight through to the shim above. Wired for real:
  `edit_add_track`, `edit_remove_track`, `edit_list_tracks`, `project_save`,
  `project_get_state`/`project_select` (undo/redo depth read from the real
  `QUndoStack`).
- **Process startup** (`shotcut/src/main.cpp`): opt-in only — if
  `SNAPSHOT_SAP_SOCKET` is unset, this process behaves exactly like stock
  Shotcut, no socket is ever opened. If set, once `MainWindow` exists and is
  shown, a dedicated background `std::thread` (never the Qt main thread)
  calls the Rust-exported `sap_start_server()`, which builds a tokio runtime
  and runs `server::serve()` against a real `FfiBackend` — this is the
  actual "Rust layer runs inside the Qt process" integration point; without
  it, everything else here is inert inside a real Shotcut process.
  `SNAPSHOT_HEADLESS=1` sets `QT_QPA_PLATFORM=offscreen` before
  `Application`/`QApplication` is constructed (has to happen before
  construction — QPA platform selection is resolved at that point).
- **Build integration**: `shotcut/CMakeLists.txt` additively pulls in
  [corrosion](https://github.com/corrosion-rs/corrosion) via `FetchContent`
  and calls `corrosion_import_crate(... FEATURES real_ffi)` against this
  crate's `Cargo.toml`, exposing a `sap_rust` CMake target (a static
  library — `Cargo.toml`'s `[lib] crate-type` now includes `staticlib`
  alongside the default `rlib`) that `shotcut/src/CMakeLists.txt` links into
  the `shotcut` target, alongside the new `rustbridge/sap_ffi.cpp` source.

### What's still stubbed

- **`project_exit`**: idempotent no-op, same documented choice as
  `MockBackend`/`server.rs` — there's no real primitive that should mean
  "the agent asked to exit" while a live GUI session might still be in use.

That is now the only deliberately-unwired method left: `edit_append_clip`,
`edit_list_clips`, `playback_seek`, `notes_get_text`/`notes_set_text`,
`project_undo`/`project_redo`, and Qt-to-SAP notification fan-out
(`sap_emit_event` really does forward into `sap_ffi_notify_bridge` ->
`server.rs`'s per-project broadcast channel now, logging `[sap_ffi] event:
...` to stderr at the C++ call site on every real Shotcut edit -- SAP or
GUI-originated) are all wired to real primitives as of this pass; see
`ffi_backend.rs` for each method's call site. This list should be
re-verified by re-reading the code before being trusted again -- it has
drifted stale before.

### Build/test commands

```sh
# sap-rust alone (default MockBackend, no Qt/CMake involved):
source "$HOME/.cargo/env"
cd sap-rust
cargo build && cargo test                 # baseline, must stay green
cargo build --features real_ffi           # compiles the FfiBackend/shim
                                           # declarations; links fine
                                           # standalone since nothing in
                                           # sap-rust's own bin/tests calls
                                           # sap_start_server itself.

# Full embedded build, from the repo root:
cmake -S shotcut -B shotcut/build -G Ninja -DCMAKE_BUILD_TYPE=Debug
cmake --build shotcut/build -j$(nproc)
```
