# Min plan: MCP play/pause + track compositing

**Worktree:** `/home/siraj/Desktop/codebases/prv/multimedia_agent/multi_media_main-mcp-play-composite`  
**Branch:** `feat/mcp-play-composite` @ local `main` (`7a6652ea`)  
**Scope:** minimum tools so agents can (1) start/stop editor playback and (2) enable multi-track video overlay (qtblend) without pre-compositing offline.

## Stack (what actually owns the tools)

| Layer | Role | Path |
|-------|------|------|
| MCP tools | Typed agent surface | `snapshotd/internal/mcpadapter/tools_*.go` + `mcpadapter.go` `supportedSAPMethods` |
| SAP JSON-RPC | Method dispatch + `Backend` trait | `sap-rust/src/server.rs`, `backend.rs`, `ffi_backend.rs`, `ffi.rs` |
| C++ bridge | Real Shotcut mutation | `shotcut/src/rustbridge/sap_ffi.{h,cpp}` |
| Shotcut core | Real primitives | `Player::{play,pause,stop,seek}`, `MultitrackModel::setTrackComposite` |

**panel-rust is not the tool owner.** It only hosts Shotcut + talks to snapshotd (`panel-rust/src/snapshotd_client.rs`). No new panel-rust APIs required unless we want UI chrome; agent path is MCP → sap → FFI.

---

## What already exists

### Playback
| Surface | Status |
|---------|--------|
| `playback.seek` | MCP + SAP + `sap_playback_seek` → `Player::seek` |
| `playback.getFrame` | MCP + SAP + FFI (one-shot render; can move playhead) |
| `playback.play` / `pause` / `stop` / `getState` | **Missing everywhere** |

Shotcut already has the UI primitives: `Player::play(double speed)`, `pause(int position)`, `stop()`, `position()` (`player.cpp` / `player.h`). Transport is the same path the Play button uses (`played` / `paused` signals → MLT controller).

### Track compositing / overlay
| Surface | Status |
|---------|--------|
| `edit.addTrack` / `listTracks` / `setTrackProperties` | Present (mute/hide/lock/**blendMode**) |
| `sap_get/set_track_blend_mode` | Present — sets qtblend **`compositing`** (or cairoblend `"1"`) via `ChangeBlendModeCommand` |
| Track-level **composite on/off** (`disable` on blend transition) | **Missing from FFI / SAP / MCP** |

Shotcut already has the primitive agents need:

```cpp
// multitrackmodel.cpp
void MultitrackModel::setTrackComposite(int row, bool composite) {
    // getVideoBlendTransition → transition->set("disable", !composite);
}
```

Roles already exist in the model: `IsCompositeRole` (`"composite"`).

**Why emojis “disappeared”:** multi-track overlay needs an enabled `qtblend` (or movit/cairoblend) transition between video tracks. `blendMode` alone does **not** clear `disable`. Bottom V-track is intentionally non-composite; upper V-tracks must have composite **enabled**. Current MCP can create tracks and set blend mode, but cannot flip the composite enable bit the Track Properties UI exposes.

Also note: when a blend transition is disabled, `sap_get_track_blend_mode` clears the mode string — agents cannot distinguish “mode 0” from “composite off” cleanly today.

`transitions.addCrossfade` is **clip-adjacent** A/B fade, not per-track overlay compositing. Different problem.

---

## Missing toolset (minimum)

### A. Playback control (3–4 tools)

| MCP / SAP method | Params | C++ target |
|------------------|--------|------------|
| `playback.play` | `speed?` (default `1.0`) | `Player::play(speed)` (same thread pattern as `sap_playback_seek`) |
| `playback.pause` | `position?` (optional frame; default current) | `Player::pause(position)` |
| `playback.stop` | `{}` | `Player::stop()` |
| `playback.getState` *(optional but cheap)* | `{}` → `{playing, position, duration, speed?}` | `Player::position()` + playing flag (from player/controller state) |

**Why not only seek/getFrame:** agents need continuous timeline playback in the editor; `getFrame` is for visual inspect/export sampling and is not a substitute for transport.

### B. Track composite enable (1 tool + 1 field)

| MCP / SAP method | Params | C++ target |
|------------------|--------|------------|
| Extend `edit.setTrackProperties` with `composite?: bool` **or** add `edit.setTrackComposite` | `trackIndex`, `composite` | `MultitrackModel::setTrackComposite(row, composite)` |
| Extend `Track` / `edit.listTracks` | add `composite: bool` | Read `IsCompositeRole` / `!transition->get_int("disable")` in listTracks FFI |

**Prefer extending `setTrackProperties`** (already partial-update for track flags) so agents don’t learn a second track-mutation entrypoint. Mirror mute/hidden/locked/blendMode pattern.

**Semantics to document in tool description:**
- Bottom video track: composite is usually false by design (nothing under it).
- Upper video tracks: set `composite: true` after `edit.addTrack` when overlaying.
- `blendMode` is the QPainter/compositing *mode*; `composite` is the master enable (`disable` bit).
- Setting `blendMode` should ideally **not** auto-enable composite (keep parity with Shotcut UI); agents set both when overlaying.

Optional follow-up (not min): `edit.ensureTrackBlend` that plants a missing qtblend if a project loaded without one — only if listTracks can report “no blend transition” as distinct from `composite: false`.

---

## Implementation slices (thin vertical, in order)

### Phase 0 — worktree hygiene
- Init `shotcut` (and `memory` if needed) submodule in this worktree, or develop FFI against parent checkout then copy/commit into submodule branch.
- Keep changes on `feat/mcp-play-composite`.

### Phase 1 — Playback (FFI → SAP → MCP)
1. **`sap_ffi.h/.cpp`:** add `sap_playback_play`, `sap_playback_pause`, `sap_playback_stop` (+ optional `sap_playback_get_state`). Mirror `sap_playback_seek` thread affinity (`BlockingQueuedConnection` when off GUI thread).
2. **`sap-rust`:** `ffi.rs` decls; `Backend::{playback_play,pause,stop[,get_state]}`; `FfiBackend` + `MockBackend`; `server.rs` dispatch; integration tests (mock is enough for dispatch).
3. **`snapshotd` mcpadapter:** register tools in `tools_playback_notes.go`; add to `supportedSAPMethods`; output types if getState returns a struct; unit smoke on tool registration schema.

### Phase 2 — Track composite (FFI → SAP → MCP)
1. **`sap_ffi`:** `sap_set_track_composite(handle, trackIndex, composite)` → `model->setTrackComposite`; extend listTracks JSON with `"composite": bool` (and keep existing blendMode read).
2. **`sap-rust`:** `Track.composite: bool`; `edit_set_track_properties(..., composite: Option<bool>)`; wire FFI; mock backend stores the flag.
3. **`snapshotd`:** extend `Track` in `output_types.go`; add `composite` bool to `edit.setTrackProperties` tool schema + method metadata.

### Phase 3 — Prove the emoji/overlay path (acceptance)
Agent script / e2e (real sap if available, else documented manual):
1. Background clip on V1.
2. `edit.addTrack` V2..Vn + emoji clips + affine/`transition.rect` keyframes.
3. For each overlay track: `edit.setTrackProperties { composite: true, blendMode: "0" }` (or normal).
4. `playback.seek(0)` + `playback.play` (or getFrame samples mid-animation).
5. Assert upper-track pixels visible in `playback.getFrame` / export (not background-only).

---

## Explicit non-goals (this min plan)

- Full transport: scrub, loop in/out, audio scrub, reverse play (can add `speed < 0` later via `play(speed)`).
- GPU movit-specific blend property surface beyond existing blendMode path.
- New panel-rust / Slint transport buttons (editor already has Play).
- Pre-composite fallback tooling (ffmpeg join) — already works offline; not an MCP gap.

---

## File touch list (expected)

```
shotcut/src/rustbridge/sap_ffi.h
shotcut/src/rustbridge/sap_ffi.cpp
sap-rust/src/ffi.rs
sap-rust/src/backend.rs
sap-rust/src/ffi_backend.rs
sap-rust/src/server.rs
sap-rust/tests/server_integration.rs   # optional
snapshotd/internal/mcpadapter/tools_playback_notes.go
snapshotd/internal/mcpadapter/tools_edit.go
snapshotd/internal/mcpadapter/output_types.go
snapshotd/internal/mcpadapter/mcpadapter.go   # supportedSAPMethods
```

---

## One-line summary

**Missing MCP toolset = playback transport (`play`/`pause`/`stop`) + track composite enable (`composite` on tracks).**  
`blendMode` and track create/list already exist; they are insufficient for multi-track overlay and continuous preview. Bind Shotcut’s existing `Player::{play,pause,stop}` and `MultitrackModel::setTrackComposite` through `sap_ffi` → `sap-rust` → snapshotd MCP — not through panel-rust.
