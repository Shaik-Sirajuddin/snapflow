#!/usr/bin/env python3
"""Minimal XTEST input driver for the real Shotcut host smoke run."""

import argparse
import json
import pathlib
import re
import time

from Xlib import X, XK, display
from Xlib.ext import xtest


# The `ChatRustDock`'s Slint viewport is a nested X11 window inside
# Shotcut's own top-level window, offset by the window chrome above/left
# of it (menu bar, toolbar, any other docks). XTEST's `MotionNotify`
# warps the pointer in *root-window-absolute* screen coordinates, not
# coordinates relative to the dock -- ground-truthed once via
# `xwininfo -root -tree` against this harness's fixed 1280x800 Xvfb
# display and PANEL_HOST_E2E_DOCK_WIDTH-forced dock (`chatrustdock.cpp`'s
# own env-var override), which reported the dock's nested window at
# root-absolute (0, 423) regardless of its forced width. Every dock-space
# coordinate this driver computes from `sidebar.slint`'s own layout
# constants must add this offset before it reaches `click`/XTEST.
DOCK_X_OFFSET = 0
DOCK_Y_OFFSET = 423


def dock_click(xdisplay, dock_x, dock_y):
    click(xdisplay, DOCK_X_OFFSET + dock_x, DOCK_Y_OFFSET + dock_y)


def keycode(xdisplay, char):
    keysym = XK.XK_space if char == " " else XK.string_to_keysym(char)
    if keysym == 0:
        raise RuntimeError("no X keysym for {!r}".format(char))
    return xdisplay.keysym_to_keycode(keysym)


def tap(xdisplay, code):
    xtest.fake_input(xdisplay, X.KeyPress, code)
    xdisplay.sync()
    time.sleep(0.05)
    xtest.fake_input(xdisplay, X.KeyRelease, code)
    xdisplay.sync()
    time.sleep(0.05)


def click(xdisplay, x, y):
    xtest.fake_input(xdisplay, X.MotionNotify, x=x, y=y)
    xtest.fake_input(xdisplay, X.ButtonPress, detail=1)
    xtest.fake_input(xdisplay, X.ButtonRelease, detail=1)
    xdisplay.sync()
    time.sleep(0.15)


def type_text(xdisplay, text):
    for char in text:
        tap(xdisplay, keycode(xdisplay, char))


# designa v2 'input' task: "continuous backspace, all a-z characters, words".
# Deterministic so the driver and the assertion agree on the final text
# without needing to inspect intermediate compose state -- same style as
# --exercise-backspace's expected_prompt[:-2] + "x" arithmetic below.
INPUT_MATRIX_ALPHABET = "abcdefghijklmnopqrstuvwxyz"
INPUT_MATRIX_BACKSPACE_COUNT = 5
INPUT_MATRIX_WORDS = " two words"
INPUT_MATRIX_EXPECTED = (
    INPUT_MATRIX_ALPHABET[: -INPUT_MATRIX_BACKSPACE_COUNT] + INPUT_MATRIX_WORDS
)


def type_input_matrix(xdisplay):
    """Types the full alphabet, several backspaces in a row (not a single
    fix-a-typo tap), then a multi-word sequence -- the three scenarios
    designa v2's 'input' task names, composed into one compose+send so the
    existing single-prompt wait_for_prompts/event-log check verifies all
    three at once."""
    type_text(xdisplay, INPUT_MATRIX_ALPHABET)
    for _ in range(INPUT_MATRIX_BACKSPACE_COUNT):
        tap(xdisplay, keycode(xdisplay, "BackSpace"))
    type_text(xdisplay, INPUT_MATRIX_WORDS)


def prompt_events(event_log):
    if not event_log.exists():
        return []
    return [
        event
        for event in (
            json.loads(line)
            for line in event_log.read_text().splitlines()
            if line.strip()
        )
        if event["method"] == "session/prompt"
    ]


def wait_for_prompts(event_log, expected):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        events = prompt_events(event_log)
        matched = [event for event in events if event["detail"] in expected]
        if {event["detail"] for event in matched} == set(expected):
            return matched
        time.sleep(0.1)
    raise RuntimeError(
        "XTEST input did not produce the expected session/prompt backend events"
    )


def wait_for_turn_end(host_log):
    if host_log is None:
        raise RuntimeError("--wait-for-turn requires --host-log")
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if host_log.exists() and "panel-rust input: turn ended thread=" in host_log.read_text(
            errors="replace"
        ):
            return
        time.sleep(0.1)
    raise RuntimeError(
        "the host sent a prompt but did not process its completed turn before restart"
    )


def wait_for_cancelled_turn_end(host_log, thread_index=0):
    """Requires the host trace's own `turn ended thread=N reason=...` line
    (see `lib.rs`'s `apply_bridge_events`) to report the *cancelled*
    reason specifically, not merely "the turn ended eventually" -- the
    reason string is `"cancelled"` (see `agent_bridge.rs`'s
    `cancel_prompt_ends_a_slow_turn_with_cancelled_stop_reason` test for
    where that exact string is pinned). A 10s deadline, well under
    `rui-mock-agent`'s own 20s safety-net timeout for a `slow `-prefixed
    prompt, doubles as a real assertion: if the Stop click missed and the
    turn only ends later via the safety net, this raises instead of
    quietly waiting long enough to look like a pass.
    """
    if host_log is None:
        raise RuntimeError("--cancel-after-send requires --host-log")
    marker = 'turn ended thread={} reason="cancelled"'.format(thread_index)
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if host_log.exists() and marker in host_log.read_text(errors="replace"):
            return
        time.sleep(0.1)
    raise RuntimeError(
        "the host did not report a cancelled turn end for thread {} within "
        "10s of the Stop click (backend session/cancel receipt alone is not "
        "sufficient evidence -- the host must also observe it)".format(
            thread_index
        )
    )


def wait_for_pending_request(host_log, thread_index=0, timeout=10):
    """Requires the host trace's own `pending request active thread=N
    method=...` line (see `lib.rs`'s `refresh_pending_request_for`)
    before a permission-card click attempt starts -- the backend's
    `session/prompt` receipt (what `wait_for_prompts` already waits for)
    is recorded synchronously on arrival, well before the mock agent's
    own relayed `session/request_permission` request round-trips back
    through the gateway and the panel's own poll-tick renders the card;
    clicking before this fires would hit *something* on screen (no
    XTEST miss signal) while `PermissionCard` isn't actually there yet.
    """
    if host_log is None:
        raise RuntimeError("--permission-decision requires --host-log")
    marker = "pending request active thread={} ".format(thread_index)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if host_log.exists() and marker in host_log.read_text(errors="replace"):
            return
        time.sleep(0.05)
    raise RuntimeError(
        "the host never reported a pending permission request for thread "
        "{} within {}s of the prompt being sent".format(thread_index, timeout)
    )


def wait_for_attachment(host_log, thread_index=0):
    if host_log is None:
        raise RuntimeError("--wait-for-attachment requires --host-log")
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if len(attachment_names(host_log)) > thread_index:
            return
        time.sleep(0.1)
    raise RuntimeError(
        "thread {} did not complete ACPX attachment before input".format(thread_index)
    )


ATTACHMENT_RE = re.compile(r"panel-rust attachment: thread=([^ ]+) ")


def attachment_names(host_log):
    if host_log is None or not host_log.exists():
        return []
    return ATTACHMENT_RE.findall(host_log.read_text(errors="replace"))


def known_thread_indices(host_log):
    """Compatibility name retained for the driver callers; the host trace
    records durable thread names, not bridge indices."""
    return set(attachment_names(host_log))


def open_second_thread(xdisplay, host_log):
    """Expand the collapsed sidebar and click "New thread", then wait for
    a freshly created thread to finish its own real ACPX attachment -- the
    same asynchronous-binding signal `wait_for_attachment` already trusts
    for thread 0, so this reuses that evidence rather than guessing at UI
    timing.

    The fixture ships with several default threads already attached at
    startup (`DEFAULT_THREAD_NAMES` in `lib.rs`), so index 1 already
    exists before any click here -- waiting on a fixed "thread=1" marker
    would pass even if the click missed the real "New thread" control
    entirely. This instead snapshots every "attachment ready thread=N"
    index already present in the host log and waits for one that is
    strictly new, so a false click (or a click that lands on an existing
    thread row instead) cannot produce a false pass.

    The sidebar defaults to collapsed (48px, per this plan's own "default
    hide the thread list" UI decision) so "New thread" is not on screen
    until the toggle in the header strip is clicked first. Coordinates
    below come directly from `sidebar.slint`'s own layout constants
    (header row height 36px, 24px square controls, 4px padding/2px
    spacing while `compact` -- true for every dock width this harness
    uses, see `lib.rs`'s `compact: width < 320`) rather than a blind
    guess, but a real Slint font/text layout still has enough per-glyph
    slack that this scans a small candidate grid around the computed
    "New thread" position instead of trusting one exact pixel.
    """
    if host_log is None:
        raise RuntimeError("open_second_thread requires --host-log")

    baseline = known_thread_indices(host_log)

    # `sidebar-toggle` is the leftmost 24x24 control in the 36px header
    # row, at `padding` (4px while compact) from the left edge.
    dock_click(xdisplay, 16, 18)
    time.sleep(0.3)

    deadline = time.monotonic() + 20
    # "New thread" sits after a stretching filler, just left of the
    # trailing gear glyph, inside the now-144px-wide expanded header --
    # scan the right two thirds of the header row so real glyph-width
    # variance in the preceding "Chats" label/filler cannot make a single
    # fixed pixel miss the control.
    candidates = [
        (x, y)
        for y in range(6, 32, 4)
        for x in range(60, 142, 4)
    ]
    for x, y in candidates:
        if known_thread_indices(host_log) - baseline:
            return
        if time.monotonic() > deadline:
            break
        dock_click(xdisplay, x, y)
        time.sleep(0.12)

    settle_deadline = time.monotonic() + max(0.0, deadline - time.monotonic()) + 2
    while time.monotonic() < settle_deadline:
        if known_thread_indices(host_log) - baseline:
            return
        time.sleep(0.1)
    raise RuntimeError(
        "XTEST input never produced a second thread's real ACPX attachment "
        "(sidebar toggle + New thread click grid exhausted; known thread "
        "indices before/after: {} / {})".format(
            sorted(baseline), sorted(known_thread_indices(host_log))
        )
    )


def select_thread_row(xdisplay, index, host_log):
    """Click an existing sidebar thread row by index. Rows are 48px tall,
    stacked below the 36px header, and their `TouchArea` covers the full
    row regardless of `expanded` (only the row's text label is gated on
    that -- see `sidebar.slint`), so this works against the default
    collapsed 48px-wide sidebar with no toggle needed. Confirms success
    from the click's own host trace, which reports the *post-click*
    `selected_thread` synchronously (Slint's `clicked` callback runs
    inside the same `dispatch_event` call the click handler makes).
    """
    if host_log is None:
        raise RuntimeError("select_thread_row requires --host-log")
    row_y = 36 + index * 48 + 24
    offset = host_log.stat().st_size if host_log.exists() else 0
    dock_click(xdisplay, 20, row_y)
    deadline = time.monotonic() + 2
    marker = "selected_thread={}".format(index)
    while time.monotonic() < deadline:
        if host_log.exists() and marker in host_log.read_text(errors="replace")[offset:]:
            return
        time.sleep(0.05)
    raise RuntimeError(
        "clicking sidebar row {} at y={} did not select it (host trace never "
        "reported {!r}; screen click was ({}, {}))".format(
            index, row_y, marker, DOCK_X_OFFSET + 20, DOCK_Y_OFFSET + row_y
        )
    )


def cancel_events(event_log):
    if not event_log.exists():
        return []
    return [
        json.loads(line)
        for line in event_log.read_text().splitlines()
        if line.strip()
    ]


def wait_for_cancel_record(event_log, session_id, deadline):
    """Polls the mock-agent backend event log until a `session/cancel`
    record for this exact session id appears (recorded synchronously by
    `rui-mock-agent`'s own `CancelNotification` handler, see
    `mock_agent.rs`), or `deadline` (a `time.monotonic()` value, not a
    duration) passes. Returns a bool rather than raising so a caller
    scanning several click candidates can keep trying instead of failing
    on the first miss.
    """
    while time.monotonic() < deadline:
        for event in cancel_events(event_log):
            if event["method"] == "session/cancel" and event.get("session_id") == session_id:
                return True
        time.sleep(0.05)
    return False


def stop_button_dock_xy(dock_width):
    """Computes the Send/Stop `TouchArea`'s dock-relative center pixel
    straight from `chat_area.slint`'s own fixed layout constants (no
    text-metrics dependency the way the sidebar's "New thread" label
    scan needed one -- this button's hit area is a fixed-size
    `Rectangle`, not text-width-driven), rather than a blind screen
    scan:

    - The dock's own top-level window is pinned to a 260px floor
      (`container->setMinimumSize(180, 260)` in `chatrustdock.cpp`,
      ground-truthed via `xwininfo -root -tree`; see this module's
      `DOCK_Y_OFFSET` doc comment) regardless of forced width, and
      `ChatArea`'s own vertical layout only stretches the message list,
      not the header or `compose-shell` -- so `compose-shell` is always
      pinned to the dock's bottom edge.
    - `compact` (true for every dock width this harness uses -- see
      `lib.rs`'s `compact: width < 320`) sets `compose-shell`'s own
      height (72px) and the button `Rectangle`'s width (44px); the
      button height (26px) and the row's fixed paddings/spacing are not
      compact-gated.
    """
    dock_height = 260
    compact = dock_width < 320
    shell_height = 72 if compact else 78
    button_width = 44 if compact else 116
    button_height = 26
    shell_top = dock_height - shell_height
    row1_height = 34
    pad_top = 8
    row_spacing = 5
    pad_right = 10
    x2 = dock_width - pad_right
    x1 = x2 - button_width
    y1 = shell_top + pad_top + row1_height + row_spacing
    y2 = y1 + button_height
    return (x1 + x2) // 2, (y1 + y2) // 2, (x1, y1, x2, y2)


def click_stop_button(xdisplay, dock_width, event_log, session_id):
    """Clicks the Send/Stop toggle while a `slow `-prefixed prompt is in
    flight, confirming success via the backend's own `session/cancel`
    receipt (not a screenshot -- see this project's standing rule) for
    the exact session the prompt used. Tries the layout-computed center
    point first, then a tight plus-shaped fallback of nearby candidates
    (covers ~1-2px of layout-rounding slack, not a blind grid) -- the
    whole attempt budget stays well under `rui-mock-agent`'s own 20s
    safety-net timeout (see `mock_agent.rs`), so a miss fails loudly
    instead of degrading into "it worked, just slowly via the timeout".
    """
    center_x, center_y, bounds = stop_button_dock_xy(dock_width)
    candidates = [(center_x, center_y)] + [
        (center_x + dx, center_y + dy)
        for dx, dy in ((-6, 0), (6, 0), (0, -6), (0, 6), (-4, -4), (4, 4))
    ]
    overall_deadline = time.monotonic() + 12
    for x, y in candidates:
        if time.monotonic() > overall_deadline:
            break
        dock_click(xdisplay, x, y)
        if wait_for_cancel_record(event_log, session_id, min(time.monotonic() + 0.6, overall_deadline)):
            return
    raise RuntimeError(
        "XTEST input never produced a session/cancel backend event for "
        "session {!r} (stop-button candidates exhausted around dock-"
        "relative bounds {})".format(session_id, bounds)
    )


def permission_decision_events(event_log):
    return [
        event
        for event in cancel_events(event_log)
        if event["method"] == "session/request_permission"
    ]


def wait_for_permission_decision(event_log, session_id, deadline):
    """Same polling contract as `wait_for_cancel_record`, for the
    `session/request_permission` record `rui-mock-agent` writes once the
    real client (the panel) answers a `permission `-prefixed prompt's
    live relay request (see `mock_agent.rs`'s doc comment on that
    marker). Returns the chosen option id string, or `None` on timeout.
    """
    while time.monotonic() < deadline:
        for event in permission_decision_events(event_log):
            if event.get("session_id") == session_id:
                return event["detail"]
        time.sleep(0.05)
    return None


def permission_button_dock_xy(dock_width, approve):
    """Computes the `PermissionCard`'s Approve/Reject `TouchArea` center
    pixel from `chat_area.slint`'s and `permission_card.slint`'s own
    fixed layout constants -- like `stop_button_dock_xy`, both buttons
    are fixed-size `Rectangle`s pinned a fixed distance above the
    dock's bottom edge (the card sits directly above the compose bar's
    separator, with nothing stretchy between them; only the message
    `Flickable` above absorbs leftover vertical space), so this is not
    text-metrics-dependent despite the card's own overall height
    varying with the summary text's word-wrap.

    Layout, bottom-up: `compose-shell` (`compact ? 72 : 78`px) pinned to
    the dock's 260px floor, a 1px separator directly above it, then the
    permission-card row (`chat_area.slint`'s own `compact ? 6 : 10`px
    `HorizontalLayout` padding around `PermissionCard`, whose own
    `VerticalLayout` -- `permission_card.slint` -- has a fixed 10px
    padding/6px spacing and a fixed 26px-tall action-button row, right-
    aligned: a stretching spacer, then Reject (72px wide), 8px spacing,
    then Approve (72px wide)).
    """
    dock_height = 260
    compact = dock_width < 320
    shell_height = 72 if compact else 78
    separator_height = 1
    outer_pad = 6 if compact else 10
    card_pad = 10
    button_row_height = 26
    button_width = 72
    button_spacing = 8

    shell_top = dock_height - shell_height
    card_row_bottom = shell_top - separator_height
    card_bottom = card_row_bottom - outer_pad
    button_row_y1 = card_bottom - card_pad - button_row_height
    button_row_y2 = button_row_y1 + button_row_height
    center_y = (button_row_y1 + button_row_y2) // 2

    card_left = outer_pad
    card_width = dock_width - 2 * outer_pad
    row_right = card_left + card_pad + (card_width - 2 * card_pad)
    approve_x2 = row_right
    approve_x1 = approve_x2 - button_width
    reject_x2 = approve_x1 - button_spacing
    reject_x1 = reject_x2 - button_width

    if approve:
        return (approve_x1 + approve_x2) // 2, center_y
    return (reject_x1 + reject_x2) // 2, center_y


def click_permission_button(xdisplay, dock_width, event_log, session_id, approve):
    """Same computed-point-first, tight-fallback-scan strategy as
    `click_stop_button`, confirmed via the backend's own recorded
    decision rather than a screenshot.
    """
    expected = "allow-once" if approve else "reject-once"
    center_x, center_y = permission_button_dock_xy(dock_width, approve)
    candidates = [(center_x, center_y)] + [
        (center_x + dx, center_y + dy)
        for dx, dy in ((-6, 0), (6, 0), (0, -6), (0, 6), (-4, -4), (4, 4))
    ]
    overall_deadline = time.monotonic() + 12
    for x, y in candidates:
        if time.monotonic() > overall_deadline:
            break
        dock_click(xdisplay, x, y)
        decision = wait_for_permission_decision(
            event_log, session_id, min(time.monotonic() + 0.6, overall_deadline)
        )
        if decision is not None:
            if decision != expected:
                raise RuntimeError(
                    "clicking the {} button produced backend decision {!r}, "
                    "expected {!r}".format(
                        "Approve" if approve else "Reject", decision, expected
                    )
                )
            return
    raise RuntimeError(
        "XTEST input never produced a session/request_permission backend "
        "decision for session {!r} ({} button candidates exhausted around "
        "({}, {}))".format(
            session_id, "Approve" if approve else "Reject", center_x, center_y
        )
    )


def transcript_lines(host_log, thread_index=0):
    if host_log is None or not host_log.exists():
        return []
    prefix = "panel-rust input: transcript thread={} ".format(thread_index)
    return [
        line
        for line in host_log.read_text(errors="replace").splitlines()
        if prefix in line
    ]


def wait_for_transcript_entries(host_log, thread_index, expectations, timeout=10):
    """Coverage-matrix "tool stream" host scenario: confirms the *typed
    reducer transcript* (not the backend's own record, and not a
    screenshot -- see `lib.rs`'s `render_messages` trace) actually
    contains each `(kind, exact_text)` pair in `expectations` before
    `timeout`. `render_messages` only traces the transcript's own tail
    (last 3 entries) on every call, but it is called on every growth
    step, so each newly-appended entry passes through that tail window
    at least once regardless of how long prior history already is --
    this only needs to scan the host log's accumulated trace lines, not
    catch one at an exact instant.
    """
    deadline = time.monotonic() + timeout
    remaining = list(expectations)
    while time.monotonic() < deadline:
        lines = transcript_lines(host_log, thread_index)
        remaining = [
            (kind, text)
            for kind, text in expectations
            if not any(
                'kind={} text="{}"'.format(kind, text) in line for line in lines
            )
        ]
        if not remaining:
            return
        time.sleep(0.1)
    raise RuntimeError(
        "host trace never showed transcript entries {} for thread {} "
        "(most recent transcript trace lines: {})".format(
            remaining, thread_index, transcript_lines(host_log, thread_index)[-10:]
        )
    )


def local_terminal_toggle_dock_xy(dock_width):
    """Computes the header's "Toggle local terminal" `TouchArea` center
    from `chat_area.slint`'s own fixed layout constants -- the header
    `Rectangle` is always present (unlike the conditionally-rendered
    permission card), so this reuses the same right-anchored reasoning
    `stop_button_dock_xy` already proved live across four dock widths:
    the header row's visible (compact, non-narrow) children are the
    thread icon (fixed 22px), a stretching spacer, the toggle (fixed
    24px), and -- easy to miss on a first read, confirmed the hard way
    against a live session where (242, 22) landed on this button
    instead -- a trailing settings-gear button (also fixed 24px) after
    it. Both trailing buttons are right-anchored, so the toggle sits
    one 24px-button-plus-5px-spacing left of the dock's right edge, not
    flush against it.
    """
    compact = dock_width < 320
    header_height = 44 if compact else 52
    pad = 6 if compact else 12
    spacing = 5
    toggle_size = 24
    settings_size = 24
    x2 = dock_width - pad - settings_size - spacing
    x1 = x2 - toggle_size
    return (x1 + x2) // 2, header_height // 2


def local_terminal_focus_dock_xy(dock_width):
    """Computes the local terminal card's "Focus local terminal" header
    row center once open. The card is fixed-height (`compact ? 120 :
    180`px) and, like the permission card, sits directly above the
    compose bar's separator with nothing stretchy between them -- same
    bottom-anchoring as `stop_button_dock_xy`/`permission_button_dock_
    xy`. Unlike the permission card, this row's own `TouchArea` has no
    inner horizontal padding beyond the card's, so a wide, safe target
    near the row's horizontal center is used rather than a tight
    button-width computation.
    """
    dock_height = 260
    compact = dock_width < 320
    shell_height = 72 if compact else 78
    separator_height = 1
    outer_pad = 6 if compact else 10
    card_height = 120 if compact else 180
    inner_pad = 8
    row_height = 18

    shell_top = dock_height - shell_height
    card_bottom = shell_top - separator_height - outer_pad
    card_top = card_bottom - card_height
    row_y1 = card_top + inner_pad
    row_y2 = row_y1 + row_height
    center_x = dock_width // 2
    return center_x, (row_y1 + row_y2) // 2


def compose_input_dock_xy(dock_width):
    """Return the center of the ChatInputLayout text input.

    ChatArea's bottom layout is: separator, ChatInputLayout, with the
    message stream taking the remaining height. ChatInputLayout's compact
    layout has 12px top/bottom padding, an input row clamped to 34px, an
    8px row gap, and a 24px selector row.
    """
    dock_height = 260
    shell_height = 12 + 34 + 8 + 24 + 12
    input_top = dock_height - shell_height + 12
    return max(12, dock_width // 2), input_top + 17


def local_terminal_events(host_log, thread_index=0):
    if host_log is None or not host_log.exists():
        return []
    prefix_toggle = "panel-rust input: local terminal toggled thread={} ".format(
        thread_index
    )
    prefix_output = "panel-rust input: local terminal output thread={} ".format(
        thread_index
    )
    prefix_key = "panel-rust input: local terminal key thread={} ".format(thread_index)
    lines = host_log.read_text(errors="replace").splitlines()
    return [
        line
        for line in lines
        if prefix_toggle in line or prefix_output in line or prefix_key in line
    ]


def wait_for_local_terminal(host_log, thread_index, marker_substring, timeout=10):
    """Polls the host trace for any local-terminal event line (toggle/
    output/key, see `lib.rs`) containing `marker_substring`.
    """
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if any(
            marker_substring in line
            for line in local_terminal_events(host_log, thread_index)
        ):
            return
        time.sleep(0.1)
    raise RuntimeError(
        "host trace never showed a local-terminal event containing {!r} "
        "for thread {} within {}s".format(marker_substring, thread_index, timeout)
    )


def focus_compose(xdisplay, x, y, host_log):
    if host_log is None:
        click(xdisplay, x, y)
        return

    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        # Dock restoration is allowed to change the vertical split. Probe a
        # compact band in the chat column, but do not type until Rust confirms
        # a click reached the actual TextInput.
        for candidate_y in (y, y + 12, y - 12, y + 24, y - 24):
            if candidate_y < 0:
                continue
            offset = host_log.stat().st_size if host_log.exists() else 0
            click(xdisplay, x, candidate_y)
            focus_deadline = time.monotonic() + 0.25
            while time.monotonic() < focus_deadline:
                if host_log.exists():
                    recent = host_log.read_text(errors="replace")[offset:]
                    if "panel-rust input: click" in recent and "compose_focus=true" in recent:
                        return
                time.sleep(0.05)
        time.sleep(0.2)
    raise RuntimeError("XTEST click never focused the restored chat composer")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--dock-width", type=int, required=True)
    parser.add_argument("--event-log", type=pathlib.Path, required=True)
    parser.add_argument("--prompt", default="host e2e prompt")
    parser.add_argument(
        "--exercise-backspace",
        action="store_true",
        help="type and remove a typo before sending the prompt",
    )
    parser.add_argument(
        "--exercise-input-matrix",
        action="store_true",
        help=(
            "designa v2 'input' task's three named scenarios in one compose: "
            "type the full a-z alphabet, continuous backspace (multiple taps "
            "in a row, not exercise-backspace's single fix-a-typo tap), then "
            "type a multi-word sequence -- overrides --prompt with the "
            "resulting deterministic text and verifies it via the same "
            "event-log session/prompt detail check every other scenario uses"
        ),
    )
    parser.add_argument(
        "--same-session-as",
        help="assert this prompt is delivered to the session used by an earlier prompt",
    )
    parser.add_argument(
        "--different-session-from",
        help=(
            "assert this prompt used a session id distinct from an earlier "
            "prompt's -- provider/thread isolation evidence"
        ),
    )
    parser.add_argument(
        "--new-thread-before",
        action="store_true",
        help=(
            "expand the sidebar and click New Thread before composing, then "
            "target the freshly created (and auto-selected) thread"
        ),
    )
    parser.add_argument(
        "--select-thread-row",
        type=int,
        help=(
            "click an existing sidebar thread row by index before composing "
            "-- provider isolation uses this against the default fixture "
            "threads instead of creating a new one"
        ),
    )
    parser.add_argument(
        "--host-log",
        type=pathlib.Path,
        help="Shotcut stderr log with RUI_PANEL_INPUT_TRACE enabled",
    )
    parser.add_argument(
        "--wait-for-turn",
        action="store_true",
        help="require host-side turn completion after backend receipt",
    )
    parser.add_argument(
        "--wait-for-attachment",
        action="store_true",
        help="require asynchronous ACPX session attachment before input",
    )
    parser.add_argument(
        "--cancel-after-send",
        action="store_true",
        help=(
            "click the Send/Stop toggle once this prompt's session/prompt "
            "backend receipt arrives (use with a 'slow '-prefixed --prompt, "
            "which rui-mock-agent blocks on until a real session/cancel "
            "notification arrives), then require the host trace to report "
            "a cancelled (not timed-out) turn end"
        ),
    )
    parser.add_argument(
        "--permission-decision",
        choices=["approve", "reject"],
        help=(
            "click the permission card's Approve/Reject toggle once this "
            "prompt's session/prompt backend receipt arrives (use with a "
            "'permission '-prefixed --prompt, which rui-mock-agent relays "
            "as a real session/request_permission request and blocks on "
            "the real client's answer), then require the backend's "
            "recorded decision to match"
        ),
    )
    parser.add_argument(
        "--permission-pending-only",
        action="store_true",
        help="debug: wait for the pending permission card, then exit without clicking",
    )
    parser.add_argument(
        "--assert-tool-stream",
        action="store_true",
        help=(
            "require the typed reducer transcript to contain the "
            "thought/tool-call/message entries rui-mock-agent's default "
            "(non-slow, non-permission) prompt handling emits for "
            "--prompt's exact text (requires --wait-for-turn)"
        ),
    )
    parser.add_argument(
        "--local-terminal-round-trip",
        action="store_true",
        help=(
            "toggle the client-local PTY terminal open, wait for real "
            "shell output, focus it, type a marker command, wait for "
            "its real echoed output, then toggle it closed -- entirely "
            "host/client-local, no ACPX backend involvement"
        ),
    )
    args = parser.parse_args()

    xdisplay = display.Display()
    # The smoke harness uses a deterministic 1280x800 Shotcut layout. The
    # compose control sits in the chat dock's lower half. This gate drives
    # only the deterministic compose coordinate; multi-session routing is
    # covered by the real gateway actor suite, where sessions have stable IDs.
    compose_x, compose_y = compose_input_dock_xy(args.dock_width)
    compose_x += DOCK_X_OFFSET
    compose_y += DOCK_Y_OFFSET
    expected_prompt = INPUT_MATRIX_EXPECTED if args.exercise_input_matrix else args.prompt

    if args.wait_for_attachment:
        wait_for_attachment(args.host_log)
    if args.local_terminal_round_trip:
        toggle_x, toggle_y = local_terminal_toggle_dock_xy(args.dock_width)
        dock_click(xdisplay, toggle_x, toggle_y)
        wait_for_local_terminal(args.host_log, 0, "open=true")
        # A real shell prints *something* (prompt, banner, or at minimum
        # a cursor-positioning escape the VT100 parser turns into visible
        # text) within a couple seconds of spawning -- this is the one
        # signal that distinguishes a genuine PTY from a UI flag flip
        # with no process behind it.
        wait_for_local_terminal(args.host_log, 0, "local terminal output thread=0")
        focus_x, focus_y = local_terminal_focus_dock_xy(args.dock_width)
        dock_click(xdisplay, focus_x, focus_y)
        marker = "hoste2eptymarker"
        type_text(xdisplay, "echo {}".format(marker))
        tap(xdisplay, keycode(xdisplay, "Return"))
        wait_for_local_terminal(args.host_log, 0, marker, timeout=15)
        dock_click(xdisplay, toggle_x, toggle_y)
        wait_for_local_terminal(args.host_log, 0, "open=false")
        return
    if args.new_thread_before:
        open_second_thread(xdisplay, args.host_log)
    if args.select_thread_row is not None:
        select_thread_row(xdisplay, args.select_thread_row, args.host_log)
    focus_compose(xdisplay, compose_x, compose_y, args.host_log)
    if args.exercise_input_matrix:
        type_input_matrix(xdisplay)
    elif args.exercise_backspace:
        # This explicitly checks that the focused chat composer owns editing
        # keys from Shotcut: remove the injected typo before dispatching.
        type_text(xdisplay, expected_prompt[:-2] + "x")
        tap(xdisplay, keycode(xdisplay, "BackSpace"))
        type_text(xdisplay, expected_prompt[-2:])
    else:
        type_text(xdisplay, expected_prompt)
    tap(xdisplay, keycode(xdisplay, "Return"))

    matched = wait_for_prompts(args.event_log, [expected_prompt])
    current = next(event for event in reversed(matched) if event["detail"] == expected_prompt)
    if args.cancel_after_send:
        click_stop_button(xdisplay, args.dock_width, args.event_log, current["session_id"])
        wait_for_cancelled_turn_end(args.host_log)
    if args.permission_decision:
        wait_for_pending_request(args.host_log)
        click_permission_button(
            xdisplay,
            args.dock_width,
            args.event_log,
            current["session_id"],
            approve=(args.permission_decision == "approve"),
        )
    if args.permission_pending_only:
        wait_for_pending_request(args.host_log)
        print("PENDING_READY")
        return
    if args.wait_for_turn:
        wait_for_turn_end(args.host_log)
    if args.assert_tool_stream:
        wait_for_transcript_entries(
            args.host_log,
            0,
            [
                ("thinking", "considering: {}".format(expected_prompt)),
                ("tool_use", "mock_tool(input={})".format(expected_prompt)),
                ("agent", expected_prompt.upper()),
            ],
        )
    if args.same_session_as:
        reference = next(
            (
                event
                for event in reversed(prompt_events(args.event_log))
                if event["detail"] == args.same_session_as
            ),
            None,
        )
        if reference is None:
            raise RuntimeError(
                "cannot verify resumed session: prior prompt {!r} was absent".format(
                    args.same_session_as
                )
            )
        if reference["session_id"] != current["session_id"]:
            raise RuntimeError(
                "prompt {!r} used session {!r}, expected resumed session {!r} from {!r}".format(
                    expected_prompt,
                    current["session_id"],
                    reference["session_id"],
                    args.same_session_as,
                )
            )
    if args.different_session_from:
        reference = next(
            (
                event
                for event in reversed(prompt_events(args.event_log))
                if event["detail"] == args.different_session_from
            ),
            None,
        )
        if reference is None:
            raise RuntimeError(
                "cannot verify thread isolation: prior prompt {!r} was absent".format(
                    args.different_session_from
                )
            )
        if reference["session_id"] == current["session_id"]:
            raise RuntimeError(
                "prompt {!r} shared session {!r} with {!r}; expected the New "
                "Thread click to bind a distinct session".format(
                    expected_prompt, current["session_id"], args.different_session_from
                )
            )


if __name__ == "__main__":
    main()
