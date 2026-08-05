# MCP play / composite / canvas / image-sequence

**Branch / worktree:** `feat/mcp-play-composite`  
**Base:** local `main`  
**Out of scope:** SVG animation (still SVG via path import only; no dedicated tool)

## Goals

1. **Playback transport** — `playback.play` / `pause` / `stop` (+ optional `getState`)
2. **Track overlay enable** — `composite` on `edit.setTrackProperties` + listTracks
3. **Canvas / profile** — `project.setProfile` / `project.getProfile` (named profile or width/height/fps)
4. **Image sequence** — `playlist.appendImageSequence` (native qimage/pixbuf sequence, not SVG)
5. **Tests** — mock unit coverage + daemon-style `*_realsaprust_test.go` (skip without real binary)

## Stack

MCP (`snapshotd/mcpadapter`) → SAP JSON-RPC (`sap-rust`) → C++ (`shotcut/src/rustbridge/sap_ffi`) → Shotcut primitives

panel-rust: no changes.

## Design

### Playback
Wire `Player::{play,pause,stop}` same thread pattern as `sap_playback_seek`.  
`getState` returns `{playing, position, duration}` via `Player::position` + `Controller::isPaused`.

### Composite
`MultitrackModel::setTrackComposite(row, bool)` clears/sets qtblend `disable`.  
Expose as optional `composite` on `edit.setTrackProperties` and `Track.composite` in listTracks.  
Bottom V-track usually stays non-composite by Shotcut design.

### Profile / canvas
`project.setProfile`:
- `profileName` → `MainWindow::setProfile` / `Controller::setProfile`
- else `width` + `height` (+ optional `frameRateNum`/`frameRateDen`, default 25/1) applied to `MLT.profile()` then `updatePreviewProfile`

`project.getProfile` returns live width/height/fps.

### Image sequence
`playlist.appendImageSequence { path, ttl?, begin? }`:
Mirror `ImageProducerWidget` sequence mode: printf resource, `ttl`, `begin`, `shotcut_sequence`, count consecutive frames, append to playlist bin.

## Test matrix

| Case | Layer |
|------|--------|
| Mock: play/pause/stop/getState, composite partial update, set/get profile, appendImageSequence | `sap-rust` unit |
| MCP tool registration schemas | `go test` mcpadapter (no Qt) |
| Real: setProfile → getFrame dimensions; composite enable + two tracks; image sequence length; play after seek | `*_realsaprust_test.go` (skip if no rebuilt binary) |

## Non-goals

- Track-level opacity (use clip `filter.add` brightness/opacity)
- SVG animation producers
- panel-rust Play button chrome
