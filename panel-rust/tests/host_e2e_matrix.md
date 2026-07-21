# Host Event Matrix

Automated host E2E drives Shotcut through XTEST on its Xvfb display and uses
the mock ACP backend event log as its source of truth. Screenshots are not a
test gate.

| Scenario | Parallelism | Event evidence | Status |
| --- | --- | --- | --- |
| Composer prompt | One focused compose field | `session/prompt` contains the exact typed text | Proven (`PANEL_HOST_E2E_DRIVE=1`) |
| Input event matrix (designa v2 `input` task) | Full a-z alphabet, several backspaces in a row, then a multi-word sequence, all in one compose | `session/prompt` contains the deterministic post-edit text (alphabet minus the backspaced tail, plus the words) | Driver scenario added (`--exercise-input-matrix` / `PANEL_HOST_E2E_INPUT_MATRIX=1`); not yet run against a live Xvfb/Shotcut instance in this pass |
| New thread | Two threads, one provider | Two distinct `session/new` records followed by correctly bound prompts | Proven (`PANEL_HOST_E2E_NEW_THREAD=1`) |
| Provider isolation | Codex and Claude concurrently | Each backend log has only its provider's prompt marker | Proven (`PANEL_HOST_E2E_PROVIDER_ISOLATION=1`) -- uses the fixture's own default thread 1 (Claude), not a freshly clicked thread |
| Parallel turns | Two sessions concurrently | Both expected prompt markers arrive; neither blocks or crosses sessions | Pending |
| Tool stream | Prompt emits thought/tool/message updates | Backend prompt record and typed reducer transcript agree | Proven (`PANEL_HOST_E2E_TOOL_STREAM=1`) |
| Permission / FS / terminal approval | Owning and foreign client connections | One response from the owner; foreign response is rejected | Foreign-client rejection proven at the transport layer (`a_foreign_connections_forged_relay_response_is_rejected_and_the_real_owner_still_answers`); owner-side host UI click still pending, see note below |
| Agent terminal | Prompt plus live output | `terminal/create`, output deltas, exit state, and release are recorded | Pending |
| Client PTY | Two local terminals in parallel | Separate shell markers and resize observations per thread | Single-thread open/type/echo/close round trip proven (`PANEL_HOST_E2E_LOCAL_TERMINAL=1`); two-terminals-in-parallel variant still pending |
| Cancellation | Slow prompt plus Stop | Backend observes `session/cancel`; host trace reports the turn ending with `reason="cancelled"`, not the mock agent's 20s safety-net timeout | Proven (`PANEL_HOST_E2E_CANCEL=1`) |
| Settings / MCP / agent catalog | Multiple sessions with different overrides | Expected ACPX method/payload reaches the session selected in UI | Pending |
| Restart / reconnect | Host and gateway restart | Resumed session receives next prompt without an implicit close | Proven (`PANEL_HOST_E2E_RESTART=1`) |
| HTTP degraded fallback | Gateway WS unavailable then restored | Fallback audit state is present; interactive approval is unavailable | Pending |

`PANEL_HOST_E2E_NEW_THREAD=1` and `PANEL_HOST_E2E_PROVIDER_ISOLATION=1` both
require `PANEL_HOST_E2E_DRIVE=1` in the same invocation (they assert against
the `"host e2e prompt"` marker it produces on thread 0's session). They have
not yet been verified combined with `PANEL_HOST_E2E_RESTART=1` in the same
run -- creating a second thread or reselecting thread 1 changes which thread
is persisted as "selected" before a restart, which the existing restart
assertion (`--same-session-as "host e2e prompt"`) does not yet account for.
Run them in separate invocations until that interaction is deliberately
covered.

**XTEST screen-coordinate note (read before adding more sidebar-driven
scenarios):** the embedded `ChatRustDock` is a *nested* X11 window inside
Shotcut's own top-level window, not positioned at the display's root
origin -- ground-truthed via `xwininfo -root -tree` against this harness's
fixed 1280x800 Xvfb display and `PANEL_HOST_E2E_DOCK_WIDTH`-forced dock
width (`chatrustdock.cpp`'s own env-var override) as root-absolute
`(0, 423)` regardless of forced width. `host_e2e_driver.py`'s
`DOCK_X_OFFSET`/`DOCK_Y_OFFSET` constants and `dock_click` helper encode
this; any new sidebar-relative click coordinate must go through
`dock_click`, not `click`, or it will silently miss the dock and hit
Shotcut's own menu/toolbar chrome instead with no error (XTEST does not
report a miss -- the click trace this file's tests rely on simply never
appears, which looks identical to "the driver hasn't tried yet" until you
know to look for it).

**Cancellation scenario status (2026-07-16, landed):** `rui-mock-agent`
supports a `slow `-prefixed prompt that blocks until a real
`session/cancel` notification arrives (`mock_agent.rs`), proven against
the real compiled binary at the gateway layer
(`cancel_session_ends_a_real_mock_agent_slow_turn_as_cancelled` in
`gateway_actor_e2e_test.rs`) and now also at the host XTEST layer via
`PANEL_HOST_E2E_CANCEL=1`. The Send/Stop control's dock-relative pixel
was computed directly from `chat_area.slint`'s own fixed layout
constants (`stop_button_dock_xy` in `host_e2e_driver.py`) rather than a
blind scan -- unlike the sidebar's "New thread" label, this control's
hit area is a fixed-size `Rectangle`, not text-width-dependent, so the
computed center point worked on the first live attempt with no fallback
scan needed. `click_stop_button` still carries a tight plus-shaped
fallback (±6px) in case of future layout drift, bounded well under the
mock agent's 20s safety-net timeout. Evidence chain: backend
`session/cancel` record for the exact session id used by the `slow ...`
prompt, plus the host trace's own `turn ended thread=0
reason="cancelled"` line (not merely "the turn ended eventually" --
this distinguishes a real cancel from the 20s safety-net timeout, whose
reason would be `"end_turn"` instead). Run with:
`PANEL_HOST_E2E_CANCEL=1 PANEL_HOST_E2E_DOCK_WIDTH=260
panel-rust/tests/host_e2e_smoke.sh` (self-contained, no
`PANEL_HOST_E2E_DRIVE=1` dependency). Not yet run at dock widths other
than 260px or combined with `PANEL_HOST_E2E_RESTART=1`/other scenarios
in the same invocation.

Run the quick host gate with:

```bash
PANEL_HOST_E2E_DRIVE=1 PANEL_HOST_E2E_DOCK_WIDTH=260 \
  panel-rust/tests/host_e2e_smoke.sh
```

**Dock-width coverage (2026-07-16):** `PANEL_HOST_E2E_DRIVE=1` and
`PANEL_HOST_E2E_CANCEL=1` both re-verified passing (fresh state
directories) at `PANEL_HOST_E2E_DOCK_WIDTH` values `180` (the
`container->setMinimumWidth` floor), `360`, and `520`, in addition to
the `260` every other scenario in this file uses -- `360`/`520` cross
the `compact` threshold (`lib.rs`'s `compact: width < 320`) into the
non-compact layout branch, so this also cross-validates `stop_button_
dock_xy`'s `compact`-conditional geometry math against the real host,
not just the compact case every other coordinate in this file was
ground-truthed against. `PANEL_HOST_E2E_NEW_THREAD=1`/`PROVIDER_
ISOLATION=1` (sidebar-coordinate-dependent) and `PERMISSION=1` (not
landed at all yet, see above) have not been run at non-260 widths.

**Permission-approval host-click status (2026-07-16, investigated at
length, not landed):** `rui-mock-agent` now supports a `permission
`-prefixed prompt (`mock_agent.rs`) that sends a real ACP
`session/request_permission` request out to the client via `connection.
send_request(...).block_task()` and blocks indefinitely (no safety-net
timeout needed -- unlike `slow `, there is no host-side race to lose)
until the client answers; the chosen option (or `"cancelled"`/
`"no-response"`) is recorded to the backend event log the same way
`session/cancel` is. `lib.rs`'s `refresh_pending_request_for` now also
emits a `pending request active thread=N method=... window_size=WxH
scale=S compact=C narrow=N` host trace line, and `answer_pending_request`
emits `answer pending request invoked thread=N approved=B
pending_count=N` unconditionally (before its own early-return), both
specifically added to debug this scenario and left in place as
permanently useful diagnostics (harmless, gated behind the same
`RUI_PANEL_INPUT_TRACE` opt-in as every other host trace line).

The click itself is not landed. Ground-truthed via a live held-open
session (`xwininfo -root -tree`, direct XTEST clicks, and the new trace
lines) that everything upstream of the click is exactly as expected:
the dock is `260x260+0+423` (matching the documented offset), the host
trace reports `window_size=260x260 scale=1 compact=true narrow=false`
at the moment the card becomes active, and `chat_area.slint`'s/
`permission_card.slint`'s own layout constants (used by
`permission_button_dock_xy` in `host_e2e_driver.py`, the same style of
computation `stop_button_dock_xy` uses successfully for the Send/Stop
control) place the Approve button's center at dock-relative `(208,
158)`. A single direct XTEST click at exactly that point registers on
the dock (the host's own generic `click x=208 y=158 ...` trace line
fires, proving the event reaches `panel_rust_input_click` and gets
dispatched into Slint via `WindowEvent::PointerPressed`/`Released` at
that exact logical position) but `answer_pending_request` is never
invoked -- not even once, across a single point, a plus-shaped
fallback, and (in this investigation, not landed permanently) an
exhaustive grid covering effectively the entire visible dock
(`x: 100-260, y: 40-260` step 8, ~560 points). The previously-proven
Send/Stop control, at its own known-working coordinate, *also* stopped
responding while a permission card was simultaneously pending on the
same thread (no `session/cancel` was recorded), even though clicking a
sidebar thread row at the same moment worked normally (thread
selection changed) -- so input dispatch in general is not broken, only
something specific to clicks landing in the region below the message
list while a conditionally-rendered `PermissionCard` is present.

Ruled out directly, not merely assumed: a scale-factor mismatch (traced
`scale=1`), a stale/forced dock width (traced `window_size=260x260`,
`compact=true`), a `devicePixelRatio` transform bug in
`RustPanelItem::mousePressEvent` (the host's own generic click trace
already reports the *post*-transform coordinate, and it matches what
XTEST sent, so `density` is confirmed 1.0 here), and a completely
dead input path (the Send/Stop control and sidebar row selection both
work in the same live session, just not while a permission card is
active). Not yet ruled out: a hit-test/layout staleness issue specific
to this project's hand-rolled `MinimalSoftwareWindow` + manual
`request_redraw()` pipeline for *conditionally-inserted* elements
(`if pending-request.active : Rectangle { ... PermissionCard { ... } }`
in `chat_area.slint`) -- the Send/Stop control's own parent is always
present in the tree (only its color/text/`enabled` change with
`sending`), so it would not be exposed to the same class of bug even if
one exists. A component-level geometry probe (`i-slint-backend-
testing`'s `ElementHandle::absolute_position()`/`size()`) was attempted
to get a layout-engine-native ground truth independent of X11/XTEST
entirely, but the standalone test harness does not reproduce the real
host's 260px height constraint (`panel.window().set_size()` does not
force it the way the real embedding's `PhysicalSize` + `MinimalSoftware
Window::set_size` + `request_redraw()` path does), so its Y values are
not trustworthy without further harness work; its X values did match
this note's own hand-computed geometry exactly, which is at least
confirmation the horizontal math is right.

**Next-attempt plan:** instrument `chat_area.slint`'s `send-stop-button`
and `PermissionCard`'s own `approve-request-button`/
`reject-request-button` `TouchArea`s with a temporary `pointer-event =>
{ ... }` handler (Slint's lower-level pointer callback, which fires on
every phase including ones a plain `clicked` callback would swallow) to
see whether the *item* receives the event at all versus something above
it consuming it first; or fix the standalone component-test harness to
genuinely force a 260px window height (matching `panel_rust_create`'s
own `PhysicalSize`-based mechanism, not `panel.window().set_size()`) and
then walk the full element tree's absolute positions while
`pending-request.active` is true, cross-checking every element's
bounds against every other one for unexpected full-column overlap.

**Tool stream (2026-07-16):** `lib.rs`'s `render_messages` now also
traces the typed reducer transcript's own tail (last 3 entries, each as
`kind`/a 60-char text preview) whenever it renders, opt-in behind the
same `RUI_PANEL_INPUT_TRACE` flag as every other host trace line.
`PANEL_HOST_E2E_TOOL_STREAM=1` sends a plain (non-`slow `, non-
`permission `) prompt and requires the host trace to show all three of
`rui-mock-agent`'s default per-turn emissions (`send_replay` in
`mock_agent.rs`) with their exact expected text: a `thinking` entry
(`"considering: <prompt>"`), a `tool-call` entry
(`"mock_tool(input=<prompt>)"`), and an `agent` entry (`<PROMPT
UPPERCASED>`). Verified against a real held state directory that these
lines genuinely appear with the exact expected text (not a
vacuously-true empty-expectations bug) before folding it into the
opt-in-only default run.

**Client PTY (2026-07-16):** `lib.rs` gained three new host trace
lines, all `RUI_PANEL_INPUT_TRACE`-gated: `local terminal toggled
thread=N open=<bool> [cols=.. rows=..]` (on open/close),
`local terminal output thread=N tail="..."` (last 80 chars of the real
VT100 screen buffer whenever it changes -- proves a genuine PTY process
is behind it, not a UI flag flip), and `local terminal key thread=N
bytes="..."` (each translated keystroke actually written to the PTY's
input side). `PANEL_HOST_E2E_LOCAL_TERMINAL=1` toggles the terminal
open via the header button, waits for real shell output (a genuine
`siraj@...:~$ ` prompt was observed, not a placeholder), focuses the
card, types `echo <random-marker>`, waits for the *real* echoed
marker text to come back through the actual output trace (not the key
trace -- this proves the shell executed the command, not merely that
keystrokes were sent), then toggles the terminal closed. Entirely
host/client-local, no ACPX backend involvement at all, so no `--prompt`
argument or session flow the way every ACPX-touching scenario needs.

One real coordinate bug caught live: `local_terminal_toggle_dock_xy`'s
first version assumed the toggle button was flush against the header's
right edge, but `chat_area.slint`'s header row has a trailing
"Open chat settings" gear button *after* the terminal toggle (also
24px, 5px spacing) -- missed on a first read of the `.slint` source,
caught by a live click at the wrong x landing on the settings button
(host trace showed a `click x=242 y=22 ...` with zero terminal-related
lines following it) rather than assumed correct from the source alone.

**Client PTY dock-width status:** `PANEL_HOST_E2E_LOCAL_TERMINAL=1`
verified passing (real shell prompt, real per-keystroke echo, real
command-output echo) at `PANEL_HOST_E2E_DOCK_WIDTH` `180` and `260`
(both `compact`). At `520` (non-compact), the terminal opens and shows
a real prompt (the toggle click and its own geometry are confirmed
correct at this width too), but the focus click never registers --
`local_terminal_focus_dock_xy`'s bottom-anchored math does not account
correctly for how the wrapping `Rectangle { height: local-terminal-
card.height; ... HorizontalLayout { padding: compact ? 6px : 10px;
... } }` in `chat_area.slint` actually resolves the card's vertical
position when the outer `Rectangle`'s height is bound directly to the
card's own height rather than derived from the layout -- the `compact`
(260px) case happened to still land inside the clickable header row's
18px height by coincidence of the smaller padding value, but the
`10px` non-compact padding pushes just far enough to miss. Not yet
root-caused precisely enough to fix outright; ruled out that this is a
dead input path in general (the toggle button, at a *different*,
correctly-computed y, works fine at this same width). Next attempt:
either compute this geometry the same "compute once, ground-truth via
one live click" way `stop_button_dock_xy` was, or reuse this project's
`i-slint-backend-testing` component-geometry-probe technique (see the
permission-approval note above) after first fixing that harness to
force the real 260px window-height floor.

**Post-TEA re-verification (2026-07-21):** After the TEA (Msg/update/Dirty/
sync) migration and a sync of `main`, the smoke gate was re-run against a
freshly rebuilt real-FFI Shotcut (isolated Xvfb `:150`, gateway `28790`,
per-run `mktemp` state). `PANEL_HOST_E2E_DRIVE=1` and
`PANEL_HOST_E2E_TOOL_STREAM=1` both PASS (exit 0) at `DOCK_WIDTH=260` --
composer prompt reaches the backend as `session/prompt`, and the tool-stream
reducer transcript matches the mock agent's three per-turn emissions. This
confirms the cold-start TEA hydration, the poll `Msg::Frame` tick, and
compose/send/stream render are correct post-TEA.

`PANEL_HOST_E2E_CANCEL=1` and `PANEL_HOST_E2E_LOCAL_TERMINAL=1` currently
FAIL, but root-caused to test-harness drift, **not** a product/TEA
regression (proven by direct screenshot inspection of a held session):

1. `chatrustdock.cpp` no longer forces the dock width. It now only does
   `view->setMinimumSize(240, 260)` -- the `PANEL_HOST_E2E_DOCK_WIDTH`
   env-var override this file's coordinate helpers assume ("chatrustdock's
   own env-var override", above) has been removed. With no forcing, the
   dock renders at its layout-determined width (~395px against the
   harness's 1280x800 Xvfb), so every *right-anchored* control (Send/Stop
   button, header terminal-toggle) sits far to the right of where the
   `dock_width=260` math clicks. The large, center-computed compose input
   still lands (which is why DRIVE/TOOL_STREAM pass), masking the issue.
2. The compose bar was extracted into `ChatInputLayout` and the header
   into `IconButton`s, moving the Send/Stop button to row 1's right edge
   (34x34) and making the terminal toggle the header's right-most child
   (no trailing settings gear). `stop_button_dock_xy`,
   `local_terminal_toggle_dock_xy`, and `local_terminal_focus_dock_xy`
   were updated here to the new layout constants, but still assume the
   dock is actually `dock_width` px wide.

Screenshot evidence that the underlying behavior is correct: driving a
`slow ` prompt in a held session showed the compose input placeholder flip
to "Agent is working..." and the Send button render its square Stop icon
(`root.sending` true) -- i.e. the loading state propagates through TEA and
the Stop control is live; the automated click simply targets the wrong x
for the unforced dock width.

**To unblock CANCEL/LOCAL_TERMINAL (and the sidebar-coordinate scenarios):**
restore a launch-time dock-width override in `chatrustdock.cpp` (read e.g.
`PANEL_HOST_E2E_DOCK_WIDTH` and `setFixedWidth` on the QQuickWidget) so the
rendered dock actually matches the `--dock-width` the driver computes
against. This is a Shotcut (C++) change and was left for a deliberate pass
rather than done as a side effect of the panel-rust TEA work.
