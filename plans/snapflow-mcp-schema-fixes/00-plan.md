# Snapflow MCP schema/tool fixes

Source: `memory/build/tasks/fixes.md`, expanded against
`/home/siraj/Desktop/edits/animated-emojis/project/snapflow-mcp-issues.md`
(the build-log that originally raised these). Verified against current
code, not just the issue doc — one item (clipId scoping) turned out to
already be fixed since that doc was written.

Three layers, in dependency order per fix: **cpp** (`shotcut/src/rustbridge/sap_ffi.cpp`,
FFI surface) → **rust** (`sap-rust/src/{ffi.rs,ffi_backend.rs,backend.rs,server.rs}`,
JSON-RPC backend) → **mcp** (`snapshotd/internal/mcpadapter/*.go`, typed tool schemas).

## Phase 1 — `generator.createColor` hexColor format support

Current state: `sap-rust/src/ffi_backend.rs::generator_create_color` passes
the `hexColor` string straight through to
`shotcut/src/rustbridge/sap_ffi.cpp:3022 sap_generator_create_color`, which
sets it verbatim as an MLT `color:` producer's `resource`. No normalization
anywhere. MLT itself is inconsistent: `#`-prefixed 8-digit hex is
`aarrggbb` (alpha first), `0x`-prefixed is `rrggbbaa` (alpha last) — same
digits, different meaning depending only on the prefix character.

- **rust**: add a hex normalizer in `ffi_backend.rs` (or a small helper
  module) that accepts `#RGB`, `#RRGGBB`, `#RRGGBBAA`, `0xRRGGBB`,
  `0xRRGGBBAA`, canonicalizes all of them to `#AARRGGBB` (defaulting alpha
  to `ff` when not given), and rejects anything else with
  `BackendError::InvalidParams`. Call it before the FFI call in
  `generator_create_color`.
- **cpp**: no change — passthrough is correct once given canonical input.
- **mcp**: update the `hexColor` description in
  `tools_generator_subtitles.go` to state the accepted formats and note
  the canonicalization (so callers don't need to know MLT's own
  inconsistency).

## Phase 2 — `edit.addTrack` arbitrary insertion index

Current state: `MultitrackModel::insertTrack(trackIndex, type)`
(`shotcut/src/models/multitrackmodel.cpp:3403`) and `InsertTrackCommand`
(`shotcut/src/commands/timelinecommands.cpp`) already support inserting a
track at an arbitrary index — this is real, exercised C++ core
functionality. But no FFI export uses it: `sap_add_video_track` /
`sap_add_audio_track` (`sap_ffi.cpp:194,210`) only ever append via
`AddTrackCommand`. `edit.addTrack`'s Rust/MCP surface (`ffi_backend.rs:653`,
`tools_edit.go:14`) only takes `kind`, no position.

- **cpp**: add `int sap_insert_track(void *mainWindowHandle, int trackIndex, int isVideo)`
  in `sap_ffi.cpp`, pushing `Timeline::InsertTrackCommand` the same way
  `sap_add_video_track`/`sap_add_audio_track` push `AddTrackCommand`.
  Return the resulting track index (mirroring the add-track FFI's
  return convention).
- **rust**: add the `sap_insert_track` binding in `ffi.rs`; extend
  `Backend::edit_add_track` (`backend.rs:275`) and its FFI impl
  (`ffi_backend.rs:653`) to take an optional `index: Option<usize>`,
  dispatching to insert-at-index when present, append when absent. Wire
  the optional param through `server.rs`'s `"edit.addTrack"` handler.
- **mcp**: add an optional `index` integer param to `edit.addTrack` in
  `tools_edit.go`, and document current insertion semantics either way
  (index 0 = topmost track, per the existing undocumented behavior noted
  in the issues doc — this needs to be stated, not changed).

## Phase 3 — selection-scoped tools + suggested custom tools

Per `fixes.md`: keep `track.enter`/`clip.enter` as the *required*
selection-scoping mechanism for tools that use implicit selection — no
regression to that contract (`tools_selection.go`'s own doc comment
already states explicit trackIndex/clipId on those *implicit* tools is
never honored as an override; that stays). Layer in the suggested
non-native tools/fixes from the issues doc on top of that:

### 3a. clipId scoping — already resolved, verify only

Re-checked against current code: `filter.add`/`filter.list`/
`filter.setProperty`/`filter.addKeyframe`/etc. in `tools_filter.go` all
require `clipId` and `server.rs`'s handlers parse it directly
(`Backend::parse_clip_id`) rather than falling back to `clip.enter`
selection. The "declared-but-ignored clipId" issue from the original
build log no longer reproduces — those filter tools are explicit-ID-only
today, not selection-scoped. **No code change**; add a regression test
(or extend an existing one) asserting `filter.add` with a `clipId` that
doesn't match the current `clip.enter` selection still targets the
passed `clipId`, to lock in the current (correct) behavior.

### 3b. Batch/bulk mutation tool

No bulk variant of any mutation tool exists today (confirmed: no
`*Many*`/`batch*` tool in any `tools_*.go`). Building N independent
animated objects costs N × (~6 sequential round trips: `edit.addTrack` →
`track.enter` → `playlist.addToTimeline` → `edit.trimClipOut` →
`clip.enter` → `filter.add` → per-keyframe `filter.addKeyframe`), forced
serial because each step consumes the prior step's returned ID.

- **rust**: add a `filter.addKeyframes` (plural) backend method taking an
  array of `{position, value, interpolation}` for one `(clipId,
  filterIndex, property)`, executing them as one FFI-side loop instead of
  N round trips. This is the highest-value/lowest-risk batch primitive
  (keyframes are the dominant per-object call count) — don't attempt a
  single mega-batch tool that composes addTrack→...→filter.add, that's
  much larger surface for less payoff.
- **cpp**: no new FFI needed if the rust loop just calls the existing
  `sap_filter_add_keyframe` FFI N times inside one `Backend` call — this
  reduces MCP round trips even though the FFI itself stays per-keyframe.
- **mcp**: add `filter.addKeyframes` to `tools_filter.go` mirroring
  `filter.addKeyframe`'s params but with a `keyframes` array.
- Also address issue #8 (selection re-entry fragility): make
  selection-scoped mutating tools return a clear `InvalidParams`/`NotFound`
  error when no `clip.enter`/`track.enter` selection is active, instead of
  silently no-op'ing or acting on stale state — audit `server.rs` handlers
  that read `self.selection` for a bare `.unwrap_or_default()`-style
  fallback and tighten those to explicit errors.

### 3c. `file.export` fps/frame-rate override

No way today to export at a different fps than the project profile
(`file_export` in `ffi_backend.rs:2128` takes `output_path`/`codec`/
`container` only). Keyframes are stored as raw frame numbers, not
timecodes, so this needs an explicit retime step, not just an export-time
flag, to avoid motion timing drift.

- **cpp**: check whether `melt`/MLT's own consumer supports a frame-rate
  override independent of profile (`mlt_profile_get_fps` vs consumer
  `fps` property) — if the export path already runs through a `melt`
  consumer, a `fps` consumer property may suffice without a retime pass.
- **rust**: add optional `fps` param to `file_export`; if a real retime
  is required (not just consumer-side fps override), scope that as a
  documented follow-up rather than silently shipping an unproven approach
  — this is genuinely the least-verified item, don't rush a wrong
  primitive in.
- **mcp**: add optional `fps` (numerator/denominator or single float) to
  `file.export` in `tools_file_jobs.go`, documenting that it changes
  export-time frame rate only (no retime of authored motion) unless the
  rust side ships a real retime path.

### 3d. Doc-only fixes

- Document `melt`'s `qtblend` compositing dependency (needed for >1
  composited video track) as a `file.export` precondition in
  `tools_file_jobs.go`'s description — surface "needs a display" rather
  than silently stalling/dropping compositing.
- Document `generator.createTitle`'s actual MLT composition (a
  transparent `color:` producer + `dynamictext` filter, not a distinct
  title-producer service) in `tools_generator_subtitles.go`, so callers
  know what other filters/tools are/aren't compatible with the result.

## Ordering rationale

Phase 3 sub-items run mcp/rust schema+validation work first (3a verify,
3d docs — cheap, no new capability), then the rust/cpp work that adds
real capability (3b batch keyframes, 3c fps — cpp touched only if 3c's
consumer-fps check pans out), batch tool last since it's the largest
surface and benefits from the selection-error-hardening done alongside
it.

## Acceptance

- Phase 1: unit tests in `sap-rust` covering each accepted hex format →
  correct canonical `#AARRGGBB` output, plus one rejection case.
- Phase 2: `edit.addTrack` with `index` inserts at that position
  (verified via `edit.listTracks` before/after), and omitting `index`
  still appends exactly as before (no regression).
- Phase 3a: regression test only, no behavior change expected.
- Phase 3b: `filter.addKeyframes` produces identical MLT state to N
  sequential `filter.addKeyframe` calls; missing-selection tools return
  typed errors instead of silent no-ops.
- Phase 3c: documented as best-effort/follow-up if a real retime isn't
  feasible in this pass — acceptance is "either shipped with tests, or
  explicitly scoped out with a written reason," not silently dropped.
- Phase 3d: description text updated, no test needed.

## Integration test phase (real instance)

Before the review/verification gates, build the real stack (shotcut cpp →
sap-rust → snapshotd) and run against a real sap-rust process rather than
the fake backend — this package already has this convention
(`*_realsaprust_test.go`: `audio_namespace_`, `logparity_`,
`sapcall_export_`, `phase_b_concurrency_`, `phase_c_isolation_`, etc. in
`snapshotd/internal/mcpadapter`). Requirements:

- All existing realsaprust-tagged tests stay green with the new tools
  present (no regression from adding `index`/`fps`/`filter.addKeyframes`
  params to existing schemas).
- New realsaprust coverage added for each phase: hexColor normalization
  round-tripped through a real `color:` producer, `edit.addTrack` with an
  explicit `index` verified via real `edit.listTracks`, `filter.
  addKeyframes` checked byte-for-byte equivalent to N real
  `filter.addKeyframe` calls, selection-error-hardening cases hitting the
  real backend's actual error paths (not a fake stub's), and `file.export`
  fps override (if shipped) checked against a real exported file's actual
  frame rate.
- This is the alignment check against real MCP behavior, not just the
  fake-backend unit tests used during phase implementation.
