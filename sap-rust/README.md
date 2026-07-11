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
"MltBackend" below for the implementor that actually does something real
with them.

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

## MltBackend (doc 11 Phase A: real MLT rendering, no Qt required)

`src/mlt_backend.rs` is a **third** `Backend` implementor, independent of both
`MockBackend` (pure in-memory) and `FfiBackend` (needs a live Shotcut/Qt
process). It's always available — the only runtime requirements are `melt`
and `ffprobe` on `PATH` (or the `MELT_BIN`/`FFPROBE_BIN` env overrides) — and
it's what `tests/mlt_export_integration.rs` exercises end to end.

### What's real

- **The in-memory project model**: tracks, clips (with real `ffprobe`-derived
  in/out frame counts, not guesses), a playlist/source-bin
  (`playlist.append`/`generator.createTitle` both populate it), attached
  filters with optional keyframes, and crossfade records — per project,
  rooted at `<projectsRoot>/<projectId>/`, matching
  `09-project-folder-layout.md`.
- **MLT XML generation**: real producers (file-backed and
  `color:`+`dynamictext`/`qtext`-filter title producers), one `<playlist>`
  per track, a combining `<tractor>`, and — for `transitions.addCrossfade` —
  a nested `<tractor>` splicing in real `luma` + `mix` MLT transitions. This
  is genuinely valid `melt`-renderable XML (see the manual validation this
  file's export path is built on: a hand-written 2-producer/1-playlist XML
  rendered via `melt ... -consumer avformat:...` produced a real 7.0s
  H.264/AAC file with visible rendered title text).
- **Multi-track video compositing**: every pair of consecutive video tracks
  gets a real `qtblend` `<transition>` in the top-level tractor (bottom
  track as `a_track`, the next video track up as `b_track`) — the same real
  primitive and bottom-up ordering `MultitrackModel::getVideoBlendTransition`/
  `addVideoTrack` use in real Shotcut's `multitrackmodel.cpp` (confirmed by
  reading that source), empirically verified against the installed
  `melt 7.36.1` by rendering a two-track probe and pixel-diffing decoded
  frames before/during/after the top track's visible window. Mid-timeline
  positioning on an overlay track (no `position` param on
  `edit.appendClip`) is done with a real transparent `color:#00000000`
  spacer clip, addressable as `{"blank": <frames>}` through the existing
  `source` tagged union — a real MLT `<blank>`-equivalent technique, not a
  wire-protocol change.
- **Chained crossfades on the same track**: `transitions.addCrossfade`
  called twice on the same track sharing a middle clip (e.g. `(0,1)` and
  `(1,2)`) is handled correctly — `build_track_playlist` computes each
  clip's head/tail overlap independently rather than walking clip-pairs and
  skipping past a consumed one, which is what an earlier version of this
  file did (it silently dropped the second crossfade). Verified by
  `tests/doc11_phase_a_full.rs`'s three-segment/two-crossfade export
  actually rendering and its duration matching the exact expected frame
  math (sum of trimmed segments minus both crossfade overlaps).
- **Subtitle burn-in**: real pixel burn-in via ffmpeg's own
  `avfilter.subtitles` MLT service (`av.filename=<srt path>`), attached as a
  filter on the top-level tractor (post-composite). This was empirically
  determined, not assumed, per doc 11's explicit instruction to test this
  rather than guess: real Shotcut's own mechanism (`subtitle_feed` filter +
  `subtitle.N.feed`/`subtitle.N.lang` **consumer** properties, see
  `shotcut/src/models/subtitlesmodel.cpp`/`encodedock.cpp`) was tested
  directly against `melt` with a real SRT file and only produced an *empty*
  placeholder `mov_text` stream (0 real packets) — that mechanism depends on
  a live Shotcut `Subtitles` QObject injecting per-frame cue text during
  rendering, which doesn't exist when driving `melt` as a bare CLI
  subprocess. `avfilter.subtitles` burns real, decodable text pixels in
  standalone, confirmed by decoding frames inside vs. outside a cue window.
- **`file.export`**: writes `project.mlt`, spawns a real `melt … -consumer
  avformat:<outputPath> vcodec=<codec> acodec=aac` subprocess with
  `DISPLAY` set (required for Qt-backed filters like `dynamictext`), and
  returns a `jobId` immediately — the render itself runs on a plain OS
  thread (not the shared single-writer dispatcher), so it never blocks
  other clients. `jobs.get` polls real subprocess exit status.
- **`playback.getFrame`**: a real single-frame `melt … in=N out=N -consumer
  avformat:<file>.png` invocation, whose actual output bytes are read and
  base64-encoded — not a placeholder.
- **`subtitles.addTrack`/`appendItem`**: real `.srt` sidecar files under
  `<projectRoot>/subtitles/trackN.srt` for storage (matching real Shotcut's
  own SRT-based `SubtitlesModel`/`Subtitles` I/O), *and* real burn-in at
  export time via the `avfilter.subtitles` mechanism described above — see
  that bullet for what was tested and why it's the real mechanism, not
  Shotcut's own player-only one.

### What's simulated / simplified (documented, not hidden)

- **No live Qt/QUndoStack**: `project_undo`/`project_redo` are depth
  counters only, same caveat `MockBackend` already carries — no real
  in-memory-model rewind happens.
- **`transitions.addCrossfade`**'s nested-tractor XML uses real, standalone
  MLT `luma` + `mix` transitions, not the literal `movit.luma_mix`/
  `"mix:-2"` service-string details `01-jsonrpc-spec.md` cites from
  `multitrackmodel.cpp` (that citation's exact call shape doesn't correspond
  to a standalone registered MLT service name usable outside that call
  site) — structurally a real, correct MLT crossfade, just not a
  byte-for-byte reproduction of `MultitrackModel::addTransition`'s internal
  command-splitting logic.
- **Fixed project frame rate** (`DEFAULT_FPS = 30`): one profile fps for the
  whole project rather than per-source detection: correct as long as
  imported sources actually are 30fps (which the test suite controls at
  generation time), an approximation otherwise.

### Running the MltBackend integration tests

Needs `melt`, `ffmpeg`, and `ffprobe` on `PATH`, plus a real (or Xvfb/VNC)
`DISPLAY` for `melt`'s Qt-backed filters (`dynamictext`, used by
`generator.createTitle`):

```sh
source "$HOME/.cargo/env"
cd sap-rust
DISPLAY=:1 cargo test --test mlt_export_integration
```

`tests/mlt_export_integration.rs` generates a synthetic `ffmpeg lavfi
testsrc`+`sine` source at test setup (no checked-in fixture), drives the real
server over a real Unix socket, and — in
`full_export_pipeline_produces_a_real_playable_file` — calls `file.export`,
polls `jobs.get` until the real `melt` subprocess finishes, then asserts
with a real `ffprobe` run that the exported file exists with the expected
H.264/AAC streams and a duration matching the title + clip length sum
(verified locally: a 150-frame title + 60-frame/2s clip at 30fps produced a
real 210-frame, exactly-7.000000s-video-duration MP4).

### Full doc 11 Phase A workflow test

`tests/doc11_phase_a_full.rs` drives the entire Phase A creative-session
scenario from `11-e2e-scenario-tests.md` in one project: a title card, three
trimmed ~1.5s highlight segments cut from one 9s source with two chained
crossfades between them, a zoom-in-from-center `affine` filter, a second
overlay video track with a slide-in `affine` + fade-out `brightness`
animation positioned mid-timeline, two burned-in subtitle cues, a real
export, and pixel-level verification of every visual claim by decoding real
`playback.getFrame` grabs (not just checking that RPC calls succeeded).
Run it the same way, needs the same `DISPLAY`:

```sh
DISPLAY=:1 cargo test --test doc11_phase_a_full -- --nocapture
```

Real numbers from a local run (also demonstrates the exact "sum of trimmed
segments minus crossfade overlaps, not the original source length" duration
math doc 11 asks for):

```text
phase A export: real ffprobe duration=8.512s codec=h264 expected=8.500s
  (255f @ 30fps = title 150f + 3x45f segments - 2x15f crossfade overlap)
zoom corner mean_abs_diff (early vs late) = 71.58
title white-text fraction: in-window=0.0377 out-of-window=0.0000
overlay deep-pink fraction in target rect: before=0.0000 during=0.9831 after=0.0000
subtitle white-glyph fraction in bottom band: in-window=0.0148 out-of-window=0.0000
```

See that test file's module doc comment for the design decisions it makes
(title placement, overlay positioning via a `{"blank": N}` spacer clip, the
subtitle-mechanism finding, why the overlay uses a solid off-palette color,
and the corner-diff zoom methodology) and for the two real MLT/melt
behaviors discovered empirically while building it, both also documented in
`mlt_backend.rs`'s module doc comment:

1. **Multi-track video compositing** needs an explicit `qtblend`
   `<transition>` between tracks — a bare `<tractor>` with multiple
   `<track>` elements and no compositing transition only shows the top
   track, confirmed by rendering a two-track probe with and without it.
2. **MLT's legacy `rect`/`mlt_geometry`-typed properties** (e.g. `affine`'s
   `transition.rect`) tween back toward the *first* keyframe's value past
   the last explicit keyframe if nothing pins the end — a 2-keyframe
   slide-in animation was observed sliding back out again with no third
   keyframe. A held end value needs an explicit keyframe at the last frame
   you want it to hold for. Numeric (non-rect) properties like
   `brightness`'s `level` do not have this quirk.

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

- **`project_undo`/`project_redo`**: return a "not wired" error. No shim
  wrapper for `QUndoStack::undo()`/`redo()` exists yet (analogous to the
  undo/redo-*depth* readers that are wired) — mechanical, not architectural,
  follow-up.
- **`edit_append_clip`**: returns a "not wired" error. The real primitive
  (`TimelineDock::append()`) reads from the system clipboard / "current
  source" rather than accepting a source parameter directly, so a faithful
  wrapper needs slightly more design (e.g. staging a producer first) than
  this pass covers.
- **`edit_list_clips`**, **`playback_seek`**, **`notes_get_text`**,
  **`notes_set_text`**: no real primitive wired; return an empty/no-op
  result rather than an error, since "no clips"/"no notes yet" are
  themselves valid real states.
- **`project_exit`**: idempotent no-op, same documented choice as
  `MockBackend`/`server.rs` — there's no real primitive that should mean
  "the agent asked to exit" while a live GUI session might still be in use.
- **Notification fan-out from Qt to SAP clients**: `sap_emit_event(const
  char* jsonPayload)` is a real, linkable `extern "C"` symbol, and
  `sap_install_notification_bridge()` really does connect it to
  `MultitrackModel::modified` (the nearest real, already-emitted aggregate
  signal) on Shotcut startup — but `sap_emit_event`'s body is currently a
  stub that just logs to stderr. It does **not** yet push into
  `server.rs`'s per-project `broadcast` channel, so a real Shotcut edit made
  outside of SAP (e.g. by the human user's mouse) does not yet reach
  connected JSON-RPC clients as an `edit.changed` notification. Wiring that
  fully requires a Rust-side global channel handle reachable from this
  C symbol (e.g. a `once_cell`/`OnceLock`-held `mpsc::Sender` set up inside
  `sap_start_server`) — flagged as follow-up, not attempted in this pass to
  keep the change scoped.

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
