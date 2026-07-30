#!/usr/bin/env python3
"""MCP-driven scenarios for the real Shotcut host smoke run.

Companion to `host_e2e_driver.py` (XTEST-based), not a replacement: this
driver talks to the Slint testing backend's own MCP server
(`i_slint_backend_testing::mcp_server`, enabled via `SLINT_MCP_PORT`) over
its Streamable HTTP transport instead of simulating raw X11 input. Element
lookups are by qualified id/accessible-label (`ChatInputLayout::compose`,
"Send now", ...), not fragile dock-relative pixel coordinates -- see
`host_e2e_driver.py`'s own `DOCK_X_OFFSET`/`DOCK_Y_OFFSET` comment for why
that pixel-math approach is fragile and why this one avoids it.

Requires: `SLINT_MCP_PORT` set on the Shotcut process this driver targets
(see `host_e2e_mcp_smoke.sh`). Uses only the standard library.
"""

import argparse
import json
import pathlib
import time
import urllib.error
import urllib.request


class McpError(RuntimeError):
    pass


class McpClient:
    def __init__(self, url):
        self.url = url
        self._next_id = 1

    def call(self, method, params=None):
        request_id = self._next_id
        self._next_id += 1
        body = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            body["params"] = params
        data = json.dumps(body).encode("utf-8")
        req = urllib.request.Request(
            self.url,
            data=data,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        with urllib.request.urlopen(req, timeout=10) as resp:
            payload = json.loads(resp.read())
        if "error" in payload:
            raise McpError(f"{method} failed: {payload['error']}")
        return payload["result"]

    def call_tool(self, name, arguments=None):
        result = self.call(
            "tools/call", {"name": name, "arguments": arguments or {}}
        )
        text = result["content"][0]["text"]
        try:
            return json.loads(text)
        except json.JSONDecodeError as error:
            raise McpError(
                f"{name} returned non-JSON tool text {text!r}; content={result.get('content')!r}"
            ) from error

    def wait_until_up(self, timeout=15):
        deadline = time.monotonic() + timeout
        last_error = None
        while time.monotonic() < deadline:
            try:
                self.call("initialize", {})
                return
            except (urllib.error.URLError, ConnectionError, McpError) as exc:
                last_error = exc
                time.sleep(0.1)
        raise RuntimeError(f"MCP server never came up at {self.url}: {last_error}")


def get_root_element(client):
    windows = client.call_tool("list_windows")
    handles = windows.get("windowHandles") or [{}]
    # A project lifecycle recreation can leave the old Slint window handle
    # in the testing backend briefly while the new window is already listed.
    # Newest-first keeps MCP operations attached to the live component.
    for window_handle in reversed(handles):
        props = client.call_tool("get_window_properties", {"windowHandle": window_handle})
        root = props.get("rootElementHandle") or {}
        if root:
            return window_handle, root
    return handles[-1], {}


def find_element_by_qualified_id(client, window_handle, qualified_id):
    result = client.call_tool(
        "find_elements_by_id",
        {"windowHandle": window_handle, "elementsId": qualified_id},
    )
    handles = result.get("elementHandles") or []
    if not handles:
        raise RuntimeError(f"element id not found: {qualified_id!r}")
    return handles[0]


def find_elements_by_accessible_label(client, root_handle, label, max_elements=600):
    tree = client.call_tool(
        "get_element_tree",
        {"elementHandle": root_handle, "maxElements": max_elements},
    )
    return [
        element
        for element in tree.get("elements", [])
        if element.get("accessibleLabel") == label
    ]


def wait_for_accessible_label(client, root_handle, label, timeout=10, max_elements=600):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            matches = find_elements_by_accessible_label(
                client, root_handle, label, max_elements
            )
        except McpError as error:
            # Slint invalidates all element handles when a component tree is
            # recreated during project/thread lifecycle work. Reacquire the
            # root and continue polling; this is a transport-level retry,
            # not permission to hide arbitrary MCP failures.
            if "Invalid element handle" not in str(error):
                raise
            root_handle = get_root_element(client)[1]
            time.sleep(0.1)
            continue
        if matches:
            return matches[0]["handle"]
        time.sleep(0.2)
    try:
        diagnostics = client.call_tool(
            "get_element_tree", {"elementHandle": root_handle, "maxElements": max_elements}
        )
        labels = sorted(
            {
                element.get("accessibleLabel")
                for element in diagnostics.get("elements", [])
                if element.get("accessibleLabel")
            }
        )
    except Exception as error:
        labels = [f"diagnostics failed: {error}"]
    raise RuntimeError(
        f"no element with accessibleLabel={label!r} appeared in time; labels={labels[:80]!r}"
    )


def wait_for_accessible_label_prefix(client, root_handle, prefix, timeout=10, max_elements=600):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            tree = client.call_tool(
                "get_element_tree", {"elementHandle": root_handle, "maxElements": max_elements}
            )
        except McpError as error:
            if "Invalid element handle" not in str(error):
                raise
            root_handle = get_root_element(client)[1]
            time.sleep(0.1)
            continue
        for element in tree.get("elements", []):
            label = element.get("accessibleLabel")
            if label and label.startswith(prefix):
                return element["handle"], label
        time.sleep(0.2)
    raise RuntimeError(f"no element with accessibleLabel starting {prefix!r} appeared in time")


def accessible_label_count(client, root_handle, label, max_elements=600):
    return len(find_elements_by_accessible_label(client, root_handle, label, max_elements))


def wait_for_accessible_label_count(client, root_handle, label, minimum, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if accessible_label_count(client, root_handle, label) >= minimum:
                return
        except McpError as error:
            if "Invalid element handle" not in str(error):
                raise
            root_handle = get_root_element(client)[1]
        time.sleep(0.2)
    raise RuntimeError(
        f"expected at least {minimum} elements with accessibleLabel={label!r}"
    )


def wait_for_accessible_label_count_at_most(client, root_handle, label, maximum, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if accessible_label_count(client, root_handle, label) <= maximum:
                return
        except McpError as error:
            if "Invalid element handle" not in str(error):
                raise
            root_handle = get_root_element(client)[1]
        time.sleep(0.2)
    raise RuntimeError(
        f"expected at most {maximum} elements with accessibleLabel={label!r}"
    )


def set_text(client, element_handle, text):
    client.call_tool(
        "set_element_value", {"elementHandle": element_handle, "value": text}
    )


def click(client, element_handle):
    client.call_tool("click_element", {"elementHandle": element_handle})


def press_return(client, window_handle):
    # ChatInputLayout's compose TextInput calls send-requested()
    # unconditionally on a bare Return keypress (chat_input_layout.slint),
    # regardless of ThreadState -- unlike the send/stop toggle button,
    # whose bound callback flips to stop-requested() the instant a turn
    # is in flight. Driving submission via Return avoids that toggle-
    # button ambiguity entirely. `dispatch_key_event`'s `text` is passed
    # straight through to Slint's own WindowEvent::KeyPressed{text} --
    # Return's actual wire representation is U+000A (see i-slint-common's
    # `key_codes.rs`: `'\u{000a}' # Return`), not the word "Return".
    client.call_tool(
        "dispatch_key_event", {"windowHandle": window_handle, "text": "\n"}
    )


def prompt_events(event_log: pathlib.Path):
    if not event_log.exists():
        return []
    events = []
    for line in event_log.read_text().splitlines():
        if not line.strip():
            continue
        try:
            events.append(json.loads(line))
        except json.JSONDecodeError:
            # A concurrent writer can be mid-flush on the last line when
            # this reads the file -- every caller here already polls in
            # a loop (wait_for_prompt_texts and friends), so skipping one
            # still-partial line for one read is harmless; the next poll
            # sees it complete. Only ever the tail line in practice.
            continue
    return events


def wait_for_prompt_texts(event_log, expected_texts, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        events = [
            event for event in prompt_events(event_log) if event["method"] == "session/prompt"
        ]
        seen = [event["detail"] for event in events]
        if all(text in seen for text in expected_texts):
            return events
        time.sleep(0.1)
    raise RuntimeError(
        f"expected session/prompt texts {expected_texts!r} not all observed; saw {seen if 'seen' in dir() else '?'}"
    )


def scenario_send_now(args):
    client = McpClient(args.mcp_url)
    client.wait_until_up()
    window_handle, root_handle = get_root_element(client)

    # Production cold-starts intentionally have no synthetic chat session
    # without a project identity. Create the deferred session through the
    # real UI so this queue scenario is valid for both a fresh panel and a
    # restored panel, rather than assuming a fixture thread exists.
    open_new_thread(client, window_handle, root_handle, timeout=args.timeout)
    window_handle, root_handle = get_root_element(client)

    compose_handle = find_element_by_qualified_id(
        client, window_handle, "ChatInputLayout::compose"
    )
    # dispatch_key_event routes to the window's current keyboard focus,
    # unlike set_element_value (which sets content directly regardless of
    # focus) -- click first so Return actually reaches the compose
    # TextInput's own key handler instead of going nowhere.
    click(client, compose_handle)

    # Turn 1: "slow " is mock_agent.rs's own marker for a turn that blocks
    # (up to 20s, or until a real session/cancel arrives) instead of
    # resolving immediately -- without it the mock agent replies so fast
    # that turn 2 below would never actually catch the thread in
    # ThreadState::Loading, and SendRequested would just send it directly
    # instead of enqueuing (no QueuedMessageBar, nothing to steer).
    set_text(client, compose_handle, "slow scenario turn one")
    press_return(client, window_handle)
    wait_for_prompt_texts(args.event_log, ["slow scenario turn one"])

    # Turns 2 and 3: composed while turn 1 is still in flight --
    # SendRequested's Loading-state branch enqueues instead of sending
    # immediately. Two queued entries, not one: models.rs's
    # append_send_queue_rows marks the *front* queued row `sending`
    # whenever a generation is in flight (it already shows a Stop
    # control, mirroring can_edit's same front-row exclusion) and
    # can_send_now is deliberately false there too -- steering only
    # applies to a row that isn't already the one about to auto-drain
    # next. Queue a second entry and click send-now on *that* one.
    set_text(client, compose_handle, "scenario turn two queued")
    press_return(client, window_handle)
    set_text(client, compose_handle, "scenario turn three steer me")
    press_return(client, window_handle)

    send_now_handle = wait_for_accessible_label(client, root_handle, "Send now")
    client.call_tool("start_event_recording", {})
    click(client, send_now_handle)

    # send_now cancels turn 1 (a real session/cancel, which unblocks the
    # mock agent's 20s wait immediately) and sends turn 3 right away (see
    # update.rs's ComposeMsg::QueueSendNow handler), jumping it ahead of
    # turn 2, which stays queued -- both dispatched texts must reach the
    # real backend as distinct session/prompt calls.
    wait_for_prompt_texts(
        args.event_log,
        ["slow scenario turn one", "scenario turn three steer me"],
        timeout=args.timeout,
    )
    recording = client.call_tool("stop_event_recording", {})

    sent_texts = {event["detail"] for event in prompt_events(args.event_log)}
    if "scenario turn two queued" in sent_texts:
        raise RuntimeError(
            "turn two was sent -- send_now must skip over it, not drain it"
        )

    print(
        f"PASS send_now scenario: turn one + turn three reached the backend, "
        f"turn two correctly still queued "
        f"(recorded {len(recording.get('events', []))} events during the click)"
    )


def scenario_fast_track(args):
    """SCNA-03: pressing Return on an *empty* compose box right after
    queuing a message (while a turn is still in flight) fast-tracks that
    queued entry instead of sending an empty prompt -- verifies the
    compose-text-empty branch in chat_input_layout.slint's Return-key
    handler actually reaches update()'s new QueueFastTrack arm on a real
    host, cancelling the in-flight turn and sending the queued text.
    """
    client = McpClient(args.mcp_url)
    client.wait_until_up()
    window_handle, root_handle = get_root_element(client)

    open_new_thread(client, window_handle, root_handle, timeout=args.timeout)
    window_handle, root_handle = get_root_element(client)

    compose_handle = find_element_by_qualified_id(
        client, window_handle, "ChatInputLayout::compose"
    )
    click(client, compose_handle)

    # Turn 1: blocks (mock_agent.rs's "slow " marker) so the queued entry
    # below actually enqueues instead of sending immediately.
    set_text(client, compose_handle, "slow scenario fast track base")
    press_return(client, window_handle)
    wait_for_prompt_texts(args.event_log, ["slow scenario fast track base"])

    # Enqueue one entry (arms can_fast_track), then clear the compose box
    # and press Return again with it empty -- must fast-track, not send
    # an empty prompt.
    set_text(client, compose_handle, "fast tracked message")
    press_return(client, window_handle)
    set_text(client, compose_handle, "")
    press_return(client, window_handle)

    wait_for_prompt_texts(
        args.event_log,
        ["slow scenario fast track base", "fast tracked message"],
        timeout=args.timeout,
    )
    sent_texts = [
        event["detail"]
        for event in prompt_events(args.event_log)
        if event["method"] == "session/prompt"
    ]
    if "" in sent_texts:
        raise RuntimeError(
            f"an empty prompt was sent to the backend -- fast-track did not intercept "
            f"the empty-compose Return, saw prompts: {sent_texts!r}"
        )
    print(
        "PASS fast_track scenario: empty-compose Return fast-tracked the queued "
        "message instead of sending an empty prompt"
    )


def scenario_queue_auto_drain(args):
    """Verify the server-owned queue is visible, then drains after a turn."""
    client = McpClient(args.mcp_url)
    client.wait_until_up()
    window_handle, root_handle = get_root_element(client)
    open_new_thread(client, window_handle, root_handle, timeout=args.timeout)
    window_handle, root_handle = get_root_element(client)
    compose_handle = find_element_by_qualified_id(
        client, window_handle, "ChatInputLayout::compose"
    )
    click(client, compose_handle)

    set_text(client, compose_handle, "slow auto-drain turn one")
    press_return(client, window_handle)
    wait_for_prompt_texts(args.event_log, ["slow auto-drain turn one"], timeout=args.timeout)

    set_text(client, compose_handle, "auto-drain queued turn two")
    press_return(client, window_handle)
    # The front queued row is rendered as "Stop sending" while the active
    # turn is still in flight; later rows use "Cancel queued message".
    wait_for_accessible_label_count(
        client, root_handle, "Stop sending", 1, timeout=args.timeout
    )

    # The slow mock turn completes on its own. The ACPX dispatcher must then
    # issue the queued prompt without another UI action.
    wait_for_prompt_texts(
        args.event_log,
        ["slow auto-drain turn one", "auto-drain queued turn two"],
        timeout=max(args.timeout, 30),
    )
    wait_for_accessible_label_count_at_most(
        client, root_handle, "Stop sending", 0, timeout=args.timeout
    )
    wait_for_accessible_label_count_at_most(
        client, root_handle, "Cancel queued message", 0, timeout=args.timeout
    )
    print(
        "PASS queue_auto_drain scenario: queued row appeared in Slint MCP, "
        "was sent automatically after the active turn, and disappeared"
    )


def scenario_queue_preload(args):
    """Leave a paused, visible server queue for the shell restart phase."""
    client = McpClient(args.mcp_url)
    client.wait_until_up()
    window_handle, root_handle = get_root_element(client)
    open_new_thread(client, window_handle, root_handle, timeout=args.timeout)
    window_handle, root_handle = get_root_element(client)
    compose_handle = find_element_by_qualified_id(
        client, window_handle, "ChatInputLayout::compose"
    )
    click(client, compose_handle)
    set_text(client, compose_handle, "slow restart preload turn one")
    press_return(client, window_handle)
    wait_for_prompt_texts(args.event_log, ["slow restart preload turn one"], timeout=args.timeout)
    set_text(client, compose_handle, "restart preloaded queued turn two")
    press_return(client, window_handle)
    wait_for_accessible_label_count(
        client, root_handle, "Stop sending", 1, timeout=args.timeout
    )
    print("PASS queue_preload scenario: active turn and visible queued row prepared for restart")


def scenario_queue_after_restart(args):
    """Validate queue snapshot/reconnect projection after server restart."""
    client = McpClient(args.mcp_url)
    client.wait_until_up()
    window_handle, root_handle = get_root_element(client)
    wait_for_accessible_label_count(
        client, root_handle, "Stop sending", 1, timeout=max(args.timeout, 30)
    )
    wait_for_prompt_texts(
        args.event_log,
        ["slow restart preload turn one", "restart preloaded queued turn two"],
        timeout=max(args.timeout, 30),
    )
    wait_for_accessible_label_count_at_most(
        client, root_handle, "Stop sending", 0, timeout=args.timeout
    )
    wait_for_accessible_label_count_at_most(
        client, root_handle, "Cancel queued message", 0, timeout=args.timeout
    )
    print(
        "PASS queue_after_restart scenario: resumed UI received the persisted queue, "
        "dispatched it after resume, and removed the row"
    )


def scenario_queue_background(args):
    """Verify a queue callback remains visible after switching chats."""
    client = McpClient(args.mcp_url)
    client.wait_until_up()
    window_handle, root_handle = get_root_element(client)
    open_new_thread(client, window_handle, root_handle, timeout=args.timeout)
    window_handle, root_handle = get_root_element(client)
    compose_handle = find_element_by_qualified_id(
        client, window_handle, "ChatInputLayout::compose"
    )
    click(client, compose_handle)
    set_text(client, compose_handle, "slow background turn one")
    press_return(client, window_handle)
    wait_for_prompt_texts(args.event_log, ["slow background turn one"], timeout=args.timeout)
    set_text(client, compose_handle, "background queued turn two")
    press_return(client, window_handle)
    wait_for_accessible_label_count(
        client, root_handle, "Stop sending", 1, timeout=args.timeout
    )

    # Create/select another chat, then return to the first one. The first
    # queue row must come from the subscribed background-session projection,
    # not a local file reload tied only to the selected chat.
    open_new_thread(client, window_handle, root_handle, timeout=args.timeout)
    window_handle, root_handle = get_root_element(client)
    expand_handle = wait_for_accessible_label(
        client, root_handle, "Expand thread sidebar", timeout=args.timeout
    )
    click(client, expand_handle)
    first_thread = wait_for_accessible_label(client, root_handle, "New thread 1", timeout=args.timeout)
    click(client, first_thread)
    collapse_handle = wait_for_accessible_label(
        client, root_handle, "Collapse thread sidebar", timeout=args.timeout
    )
    click(client, collapse_handle)
    window_handle, root_handle = get_root_element(client)
    wait_for_accessible_label_count(
        client, root_handle, "Stop sending", 1, timeout=args.timeout
    )
    wait_for_prompt_texts(
        args.event_log,
        ["slow background turn one", "background queued turn two"],
        timeout=max(args.timeout, 30),
    )
    wait_for_accessible_label_count_at_most(
        client, root_handle, "Stop sending", 0, timeout=args.timeout
    )
    print(
        "PASS queue_background scenario: background queue state survived chat switching, "
        "auto-dispatched, and cleared in the selected UI"
    )


def scenario_rename(args):
    """Round-trips a real thread rename through the actual host process:
    click Rename thread, type a new name, confirm, verify the header
    title element actually updates. Exercises
    offload_state_effects_off_ui_thread's RenameThread path end to end --
    the effect now does its blocking PanelStateStore write on a spawned
    std::thread rather than inline in execute_effects, then re-enters via
    slint::invoke_from_event_loop; this proves that re-entry actually
    reaches the real Slint UI/event loop, not just a unit-test model.

    Note: this does not force the write itself to fail (there is no
    reliable way to make an already-open rusqlite connection's writes
    fail on demand without risking flakiness in this harness); the
    failure branch (StateEffectFailed -> Dirty::Error) is covered by
    update.rs's state_effect_failed_surfaces_as_dirty_error_not_silently_
    dropped unit test instead. This scenario covers the success path the
    unit tests can't: a real background-thread write landing back on a
    live Qt/Slint UI.
    """
    client = McpClient(args.mcp_url)
    client.wait_until_up()
    window_handle, root_handle = get_root_element(client)

    new_name = "mcp renamed thread"
    rename_handle = wait_for_accessible_label(client, root_handle, "Rename thread")
    click(client, rename_handle)

    name_input_handle = wait_for_accessible_label(client, root_handle, "Thread name")
    set_text(client, name_input_handle, new_name)
    press_return(client, window_handle)

    wait_for_accessible_label(client, root_handle, new_name, timeout=args.timeout)
    print(f"PASS rename scenario: header title updated to {new_name!r} via the real host")


def scenario_startup_warning(args):
    """SCNA-01: forces PanelStateStore::open to fail at cold start (the
    launcher chmods the panel cache dir read-only before this process
    ever starts -- see host_e2e_mcp_smoke.sh) and verifies the failure
    reaches the live UI as a real error banner, not just stderr or a
    unit-tested Dirty::Error value.

    This exercises cold_start_error_surfacing's actual production path:
    lib.rs's panel_rust_create -> InitialState::startup_warnings ->
    update()'s InitialStateLoaded(Ok(..)) handler -> Dirty::Error{thread_id:
    "", ..} -> sync.rs (thread_id.is_empty() short-circuits the "only the
    displayed thread" filter, so this is a global banner) ->
    ChatArea.last-error -> the real "⚠ ..." Text element in chat_area.slint.
    """
    client = McpClient(args.mcp_url)
    client.wait_until_up()
    _window_handle, root_handle = get_root_element(client)

    _handle, label = wait_for_accessible_label_prefix(
        client, root_handle, "⚠ ", timeout=args.timeout
    )
    if "panel settings persistence unavailable" not in label:
        raise RuntimeError(
            f"error banner appeared but with an unexpected message: {label!r}"
        )
    print(f"PASS startup_warning scenario: real error banner appeared: {label!r}")


def scenario_mid_session_write_failure(args):
    """SCNA-10: a real, deterministic mid-session PanelStateStore write
    failure trigger, exercised end to end. Unlike scenario_startup_warning
    (which fails PanelStateStore::open itself, before this process even
    starts), the panel-state.sqlite3 connection here is already open and
    has already served at least one successful write -- this instead makes
    the *containing directory* read-only mid-session. SQLite's default
    rollback-journal mode needs to create/delete a `-journal` sibling file
    on every write even though the main .sqlite3 file itself stays
    writable, so this reliably fails the very next write with a real
    "attempt to write a readonly database" error on an already-open
    connection -- no test-only production hook needed (see the matching
    Rust-level regression,
    state_store::tests::a_mid_session_write_fails_when_the_state_dir_becomes_read_only_after_open).

    Exercises effect_executor.rs's Effect::RenameThread failure branch for
    real: the spawned-thread write fails -> StateEffectFailed -> Dirty::Error
    -> a live "⚠ failed to persist renamed chat thread: ..." banner, then
    proves the store self-heals (no code involved, just restored directory
    permissions) once the directory is writable again.
    """
    import os
    import stat

    client = McpClient(args.mcp_url)
    client.wait_until_up()
    window_handle, root_handle = get_root_element(client)

    panel_dir = args.state_dir / "panel"
    original_mode = stat.S_IMODE(panel_dir.stat().st_mode)
    os.chmod(panel_dir, 0o555)
    try:
        rename_handle = wait_for_accessible_label(client, root_handle, "Rename thread")
        click(client, rename_handle)
        name_input_handle = wait_for_accessible_label(client, root_handle, "Thread name")
        set_text(client, name_input_handle, "mcp write-failure attempt")
        press_return(client, window_handle)

        _handle, label = wait_for_accessible_label_prefix(
            client, root_handle, "⚠ ", timeout=args.timeout
        )
        # Whichever StateEffectFailed-producing write loses the race against
        # the read-only directory first (RenameThread's own write, or a
        # SetSelectedThread persist fired incidentally by opening the rename
        # dialog) is equally valid proof of the mechanism: some real,
        # already-in-flight PanelStateStore write failed mid-session and
        # reached the live UI as an error banner, not just stderr.
        if "failed to persist" not in label or "readonly database" not in label:
            raise RuntimeError(
                f"error banner appeared but with an unexpected message: {label!r}"
            )
    finally:
        os.chmod(panel_dir, original_mode)

    new_name = "mcp write-failure recovered"
    rename_handle = wait_for_accessible_label(client, root_handle, "Rename thread")
    click(client, rename_handle)
    name_input_handle = wait_for_accessible_label(client, root_handle, "Thread name")
    set_text(client, name_input_handle, new_name)
    press_return(client, window_handle)
    wait_for_accessible_label(client, root_handle, new_name, timeout=args.timeout)
    print(
        "PASS mid_session_write_failure scenario: a real mid-session "
        "PanelStateStore write failure surfaced as a live error banner, "
        "and a later write succeeded again once the directory was writable"
    )


def scenario_real_agent_smoke(args):
    """SCNA-09: the one real-agent-backend gap never actually run in this
    repo's automated (non-interactive) harnesses -- every other MCP/XTEST
    scenario in this file talks to rui-mock-agent; host_vnc_dev.sh talks to
    a real ambient-auth backend but is a manual/VNC-only harness with no
    automated pass/fail. This sends exactly one short prompt (cheapest
    model, minimal token spend -- same "one real, billed call" posture as
    this repo's own #[ignore]'d ACPX_LIVE_TEST_AMBIENT=1 acpx-server tests)
    through the real, live embedded panel UI and confirms a real assistant
    reply actually renders -- proving the whole host chain (Shotcut's
    ChatRustDock -> panel-rust's dispatch/update/sync -> a real gateway ->
    a real ambient-auth-spawned claude-acp process -> a real model
    response -> back through TEA -> a real rendered message bubble) works
    end to end, not just the gateway-level path acpx-server's own real-*
    tests already cover.
    """
    client = McpClient(args.mcp_url)
    client.wait_until_up()
    window_handle, root_handle = get_root_element(client)

    compose_handle = find_element_by_qualified_id(
        client, window_handle, "ChatInputLayout::compose"
    )
    click(client, compose_handle)
    prompt_text = "Reply with exactly the single word: OK"
    set_text(client, compose_handle, prompt_text)
    press_return(client, window_handle)

    # No backend event log for a real agent (that's rui-mock-agent-only
    # instrumentation) -- wait for a real assistant message element to
    # appear in the transcript instead, via the live element tree.
    deadline = time.monotonic() + args.timeout
    reply_text = None
    while time.monotonic() < deadline:
        tree = client.call_tool(
            "get_element_tree", {"elementHandle": root_handle, "maxElements": 800}
        )
        texts = [
            element.get("accessibleLabel", "")
            for element in tree.get("elements", [])
        ]
        candidates = [
            text for text in texts
            if text and text != prompt_text and "OK" in text.upper()
        ]
        if candidates:
            reply_text = candidates[-1]
            break
        time.sleep(0.5)
    if reply_text is None:
        raise RuntimeError(
            f"no real assistant reply appeared within {args.timeout}s -- "
            f"real-agent round trip did not complete"
        )
    print(f"PASS real_agent_smoke scenario: real assistant reply observed: {reply_text!r}")


def open_new_thread(client, window_handle, root_handle, timeout=15):
    """acpx-client-session-lease-pool: expand the collapsed sidebar, click
    "New thread", then collapse it again -- the MCP-driver equivalent of
    `host_e2e_driver.py`'s `open_second_thread`, but via accessible-label
    lookup (`sidebar.slint`'s `button-accessible-label`s) instead of blind
    pixel-coordinate scanning. Every real `+`-thread creation this drives
    goes through `AgentBridge::add_thread_with_profile_and_provider` ->
    `build_slot` -> (now) `pool_for`/`acquire_and_attach` -- so a scenario
    built on this helper genuinely exercises the pool cutover in the real
    compiled app, not just the Rust-level test suite.
    """
    expand_handle = wait_for_accessible_label(
        client, root_handle, "Expand thread sidebar", timeout=timeout
    )
    click(client, expand_handle)
    new_thread_handle = wait_for_accessible_label(
        client, root_handle, "New thread", timeout=timeout
    )
    click(client, new_thread_handle)
    collapse_handle = wait_for_accessible_label(
        client, root_handle, "Collapse thread sidebar", timeout=timeout
    )
    click(client, collapse_handle)


def send_message_in_active_thread(client, window_handle, text, timeout=15):
    """acpx-client-session-lease-pool: "New thread" alone does NOT attach a
    session -- it dispatches `Effect::NewThreadDeferred`
    (`AgentBridge::add_thread_deferred`), which opens no ACP session until
    the first message actually sends (`attach_deferred_thread`, PUI-014's
    lazy-attach optimization). Every pool scenario here needs a real
    `session/new`, so it must send a message, not just create the thread.

    Retries the compose element lookup: creating/switching a thread can
    momentarily recreate the ChatInputLayout component (same "Slint
    invalidates element handles when a component tree is recreated" race
    `wait_for_accessible_label` already handles), and `find_element_by_
    qualified_id` alone (unlike the accessible-label helpers) has no
    retry of its own.
    """
    deadline = time.monotonic() + timeout
    compose_handle = None
    last_error = None
    while time.monotonic() < deadline:
        try:
            compose_handle = find_element_by_qualified_id(
                client, window_handle, "ChatInputLayout::compose"
            )
            break
        except (McpError, RuntimeError) as error:
            last_error = error
            time.sleep(0.2)
    if compose_handle is None:
        raise RuntimeError(
            f"ChatInputLayout::compose never appeared within {timeout}s: {last_error}"
        )
    click(client, compose_handle)
    set_text(client, compose_handle, text)
    press_return(client, window_handle)


def session_id_for_prompt_text(event_log, prompt_text, timeout=15):
    """acpx-client-session-lease-pool: resolves which session id a specific
    prompt landed on, by its exact (scenario-chosen, unique) text --
    robust against unrelated `session/new` traffic elsewhere in the same
    run (e.g. `provision_mock_profile_via_admin`'s own admin-plane
    verification session), unlike counting/ordering raw `session/new`
    records directly.
    """
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for event in prompt_events(event_log):
            if event["method"] == "session/prompt" and event["detail"] == prompt_text:
                return event["session_id"]
        time.sleep(0.2)
    raise RuntimeError(f"no session/prompt with text {prompt_text!r} observed within {timeout}s")


def panel_cache_dir(args):
    """acpx-client-session-lease-pool: the panel-rust jsonl/runtime-state
    cache dir (`RUI_ACP_CACHE_DIR`, set by host_e2e_mcp_smoke.sh to
    `$state_dir/panel`). Reading `*.trailer.json`/`*.runtime.json` here is
    the same non-visual, ground-truth mechanism used to diagnose the
    "config options never populate for a pool-created thread" bug in the
    first place -- deliberately not a screenshot/element-tree guess about
    what text a dropdown shows, but the literal persisted state ChatInput's
    dropdowns are bound from.
    """
    if args.state_dir is None:
        raise RuntimeError("this scenario requires --state-dir (set by host_e2e_mcp_smoke.sh)")
    return args.state_dir / "panel"


def known_trailer_files(args):
    return set(panel_cache_dir(args).glob("*.trailer.json"))


def wait_for_new_thread_state_files(args, before, timeout=15):
    """Returns the (trailer_path, runtime_path) pair for whichever thread's
    `*.trailer.json` appeared in the panel cache dir after `before` was
    snapshotted -- the file-system analogue of `host_e2e_driver.py`'s own
    `known_thread_indices` "snapshot then diff" pattern, so a slow/racy
    write can't be mistaken for "no new thread".
    """
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        new_files = known_trailer_files(args) - before
        if new_files:
            trailer_path = sorted(new_files)[0]
            runtime_path = trailer_path.parent / trailer_path.name.replace(
                ".trailer.json", ".runtime.json"
            )
            return trailer_path, runtime_path
        time.sleep(0.2)
    raise RuntimeError(f"no new *.trailer.json appeared in {panel_cache_dir(args)} within {timeout}s")


def wait_for_non_empty_config_options(runtime_path, timeout=15):
    """Polls one thread's `*.runtime.json` for a non-empty `configOptions`
    array -- the exact field ChatInputLayout's mode/config(model)/reasoning
    `SearchableDropdown`s gate their own visibility on (`visible: root.
    config-dropdown-entries.length > 0` in chat_input_layout.slint). Empty
    here is exactly the observed bug: the dropdowns silently never render,
    while the plain current-value label elsewhere still shows *something*
    (the "just the input field value changes, other values are not
    available" symptom).
    """
    deadline = time.monotonic() + timeout
    last_seen = None
    while time.monotonic() < deadline:
        if runtime_path.exists():
            try:
                data = json.loads(runtime_path.read_text())
            except (json.JSONDecodeError, OSError):
                data = None
            if data is not None:
                last_seen = data
                if data.get("configOptions"):
                    return data
        time.sleep(0.2)
    raise RuntimeError(
        f"{runtime_path} never reported a non-empty configOptions within {timeout}s "
        f"(last seen: {last_seen!r})"
    )


def scenario_pool_new_thread_config_options_populate(args):
    """acpx-client-session-lease-pool regression test for the "switching
    the field just changes its value, but the other values needed are not
    available" bug: a thread created through the pool cutover
    (Command::AcquireAndAttach's freshly-created branch) must still end up
    with real configOptions (model/mode/effort/agent), not a permanently
    empty list. Before the fix, GatewaySessionOpener::create() discarded
    the session/new response after extracting only sessionId, so no
    capability event was ever emitted for a freshly pool-created session --
    every new thread's model/mode dropdowns silently never rendered.
    """
    client = McpClient(args.mcp_url)
    client.wait_until_up()
    window_handle, root_handle = get_root_element(client)

    before = known_trailer_files(args)
    open_new_thread(client, window_handle, root_handle, timeout=args.timeout)
    send_message_in_active_thread(client, window_handle, "pool config options scenario")

    _trailer_path, runtime_path = wait_for_new_thread_state_files(args, before, timeout=args.timeout)
    runtime_state = wait_for_non_empty_config_options(runtime_path, timeout=args.timeout)

    option_ids = {opt.get("id") for opt in runtime_state["configOptions"]}
    if "model" not in option_ids:
        raise RuntimeError(
            f"configOptions populated but missing the expected 'model' entry: "
            f"{runtime_state['configOptions']!r}"
        )
    print(
        f"PASS pool_new_thread_config_options_populate: a freshly pool-created thread's "
        f"configOptions populated with {sorted(option_ids)!r} (was permanently [] before the fix)"
    )


def scenario_pool_two_new_threads_both_populate_config_options(args):
    """Broader case: not just the first thread ever created in a run (which
    could coincidentally warm-reuse a session another test already
    populated capabilities for) -- TWO independently created threads must
    each end up with their own non-empty configOptions, proving the fix
    applies to the general pool-acquire path, not one lucky session.
    """
    client = McpClient(args.mcp_url)
    client.wait_until_up()
    window_handle, root_handle = get_root_element(client)

    seen_runtime_paths = []
    for i in range(2):
        before = known_trailer_files(args)
        open_new_thread(client, window_handle, root_handle, timeout=args.timeout)
        send_message_in_active_thread(
            client, window_handle, f"pool config options scenario thread {i}"
        )
        try:
            _trailer_path, runtime_path = wait_for_new_thread_state_files(
                args, before, timeout=args.timeout
            )
        except RuntimeError:
            # Back-to-back "New thread" clicks can race the previous
            # thread's own still-settling attach/UI state (same class of
            # timing sensitivity `pool_switch_between_threads_preserves_
            # session_routing` needed a settle-and-retry for) -- one retry
            # with a real settle pause before re-clicking is enough in
            # practice and keeps this scenario from being flaky over a
            # UI-timing artifact unrelated to the capability fix itself.
            time.sleep(1.0)
            open_new_thread(client, window_handle, root_handle, timeout=args.timeout)
            send_message_in_active_thread(
                client, window_handle, f"pool config options scenario thread {i} retry"
            )
            _trailer_path, runtime_path = wait_for_new_thread_state_files(
                args, before, timeout=args.timeout
            )
        seen_runtime_paths.append(runtime_path)
        time.sleep(0.5)

    for runtime_path in seen_runtime_paths:
        runtime_state = wait_for_non_empty_config_options(runtime_path, timeout=args.timeout)
        if not runtime_state.get("configOptions"):
            raise RuntimeError(f"{runtime_path} has empty configOptions")

    print(
        "PASS pool_two_new_threads_both_populate_config_options: both independently "
        f"created threads populated configOptions ({[p.name for p in seen_runtime_paths]!r})"
    )


def scenario_pool_two_new_threads_distinct_sessions(args):
    """acpx-client-session-lease-pool verification-matrix row "Cold first
    provider" / "Concurrent + threads": two "New thread" clicks (same
    default provider) must produce two DISTINCT `session/new` session ids
    -- proving `ProjectSessionPool::acquire`'s single-flight/no-double-
    creation guarantee holds through the real UI -> AgentBridge ->
    thread_actor -> pool -> real acpx-server -> real mock-agent chain, not
    just in pool.rs's own unit tests. Each thread's own prompt must also
    land tagged with *its own* session id, not the other thread's --
    proving `acquire_and_attach`'s exclusive lease (not a shared/aliased
    session) really is wired end to end.
    """
    client = McpClient(args.mcp_url)
    client.wait_until_up()
    window_handle, root_handle = get_root_element(client)

    text_one = "pool scenario thread one"
    text_two = "pool scenario thread two"
    open_new_thread(client, window_handle, root_handle, timeout=args.timeout)
    send_message_in_active_thread(client, window_handle, text_one)
    open_new_thread(client, window_handle, root_handle, timeout=args.timeout)
    send_message_in_active_thread(client, window_handle, text_two)

    session_one = session_id_for_prompt_text(args.event_log, text_one, timeout=args.timeout)
    session_two = session_id_for_prompt_text(args.event_log, text_two, timeout=args.timeout)
    if session_one == session_two:
        raise RuntimeError(
            f"two distinct 'New thread' clicks produced the SAME session id {session_one!r} -- "
            f"the pool handed out one session to two threads (exclusive-lease violation)"
        )
    print(
        f"PASS pool_two_new_threads_distinct_sessions: two 'New thread' clicks opened "
        f"distinct sessions {[session_one, session_two]!r} through the real pool-cutover attach path"
    )


def scenario_pool_rapid_concurrent_new_threads(args):
    """acpx-client-session-lease-pool verification-matrix row "Concurrent +
    threads" under real UI timing (near-zero think time between clicks,
    unlike the previous scenario's implicit settle time): three rapid
    "New thread" clicks must still yield three distinct session ids and no
    duplicate -- proving the pool's per-key `open_gate` single-flighting
    (acpx-client's pool.rs) actually serializes concurrent real acquires
    instead of racing two `session/new` calls for the same slot.
    """
    client = McpClient(args.mcp_url)
    client.wait_until_up()
    window_handle, root_handle = get_root_element(client)

    texts = [f"pool rapid scenario thread {i}" for i in range(3)]
    for text in texts:
        open_new_thread(client, window_handle, root_handle, timeout=args.timeout)
        send_message_in_active_thread(client, window_handle, text)

    session_ids = [
        session_id_for_prompt_text(args.event_log, text, timeout=args.timeout) for text in texts
    ]
    if len(set(session_ids)) != len(session_ids):
        raise RuntimeError(
            f"rapid 'New thread' clicks produced duplicate session ids {session_ids!r} -- "
            f"a concurrent acquire race handed out the same session twice"
        )
    print(
        f"PASS pool_rapid_concurrent_new_threads: 3 rapid clicks -> 3 distinct sessions {session_ids!r}"
    )


def scenario_pool_send_immediately_after_new_thread(args):
    """acpx-client-session-lease-pool: sending a prompt immediately after
    "New thread" (no settle time) must reach the backend on a real,
    resolvable session -- proving `Command::AcquireAndAttach` (which does
    the pool acquire + notification wiring inline, before this actor
    accepts `SendPrompt`) genuinely blocks the send until attach completes
    rather than dropping/misrouting it under real UI timing pressure with
    zero settle time between "New thread" and typing.
    """
    client = McpClient(args.mcp_url)
    client.wait_until_up()
    window_handle, root_handle = get_root_element(client)

    open_new_thread(client, window_handle, root_handle, timeout=args.timeout)

    prompt_text = "pool immediate send scenario"
    send_message_in_active_thread(client, window_handle, prompt_text, timeout=args.timeout)

    opened_session_id = session_id_for_prompt_text(
        args.event_log, prompt_text, timeout=args.timeout
    )
    print(
        "PASS pool_send_immediately_after_new_thread: immediate send after 'New thread' "
        f"(zero settle time) reached a real, correctly-attached session {opened_session_id!r}"
    )


def scenario_pool_switch_between_threads_preserves_session_routing(args):
    """acpx-client-session-lease-pool: create two threads (two distinct
    pool-acquired sessions), switch focus back to the FIRST thread, and
    send a second message there -- it must land on thread one's own
    already-acquired session, not thread two's, and must NOT trigger a
    second `session/new` for thread one (the lease stays attached across a
    focus switch; switching tabs is not a release/reacquire). Proves the
    pool's exclusive per-thread lease survives real UI navigation, not
    just a single linear create-and-send flow.
    """
    client = McpClient(args.mcp_url)
    client.wait_until_up()
    window_handle, root_handle = get_root_element(client)

    text_one = "pool switch scenario thread one first message"
    text_two = "pool switch scenario thread two"
    text_one_again = "pool switch scenario thread one second message"

    open_new_thread(client, window_handle, root_handle, timeout=args.timeout)
    send_message_in_active_thread(client, window_handle, text_one)
    session_one = session_id_for_prompt_text(args.event_log, text_one, timeout=args.timeout)

    open_new_thread(client, window_handle, root_handle, timeout=args.timeout)
    send_message_in_active_thread(client, window_handle, text_two)
    session_two = session_id_for_prompt_text(args.event_log, text_two, timeout=args.timeout)
    if session_two == session_one:
        raise RuntimeError(
            f"thread two was handed thread one's own session {session_one!r} -- "
            f"exclusive-lease violation"
        )

    # Switch back to thread one via its own sidebar row (accessible label
    # is the thread's display name -- sidebar_thread_row.slint's
    # `button-accessible-label: thread.name`) and send again.
    expand_handle = wait_for_accessible_label(
        client, root_handle, "Expand thread sidebar", timeout=args.timeout
    )
    click(client, expand_handle)
    thread_one_row = wait_for_accessible_label(
        client, root_handle, "New thread 1", timeout=args.timeout
    )
    click(client, thread_one_row)
    collapse_handle = wait_for_accessible_label(
        client, root_handle, "Collapse thread sidebar", timeout=args.timeout
    )
    click(client, collapse_handle)

    send_message_in_active_thread(client, window_handle, text_one_again, timeout=args.timeout)
    session_one_again = session_id_for_prompt_text(
        args.event_log, text_one_again, timeout=args.timeout
    )
    if session_one_again != session_one:
        raise RuntimeError(
            f"switching back to thread one and sending again landed on session "
            f"{session_one_again!r}, expected its original session {session_one!r} -- "
            f"the lease was lost/reassigned across a focus switch"
        )
    print(
        "PASS pool_switch_between_threads_preserves_session_routing: switching focus back to "
        f"thread one and sending again correctly reused its own session {session_one!r} "
        f"(not thread two's {session_two!r})"
    )


SCENARIOS = {
    "send-now": scenario_send_now,
    "fast-track": scenario_fast_track,
    "queue-auto-drain": scenario_queue_auto_drain,
    "queue-preload": scenario_queue_preload,
    "queue-after-restart": scenario_queue_after_restart,
    "queue-background": scenario_queue_background,
    "rename": scenario_rename,
    "startup-warning": scenario_startup_warning,
    "mid-session-write-failure": scenario_mid_session_write_failure,
    "real-agent-smoke": scenario_real_agent_smoke,
    "pool-two-new-threads-distinct-sessions": scenario_pool_two_new_threads_distinct_sessions,
    "pool-rapid-concurrent-new-threads": scenario_pool_rapid_concurrent_new_threads,
    "pool-send-immediately-after-new-thread": scenario_pool_send_immediately_after_new_thread,
    "pool-switch-between-threads-preserves-session-routing": scenario_pool_switch_between_threads_preserves_session_routing,
    "pool-new-thread-config-options-populate": scenario_pool_new_thread_config_options_populate,
    "pool-two-new-threads-both-populate-config-options": scenario_pool_two_new_threads_both_populate_config_options,
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mcp-url", default="http://127.0.0.1:18999/mcp")
    parser.add_argument("--event-log", type=pathlib.Path, required=True)
    parser.add_argument("--host-log", type=pathlib.Path)
    parser.add_argument(
        "--state-dir",
        type=pathlib.Path,
        help="host_e2e_mcp_smoke.sh's state_dir; only mid-session-write-failure needs this",
    )
    parser.add_argument("--timeout", type=float, default=15)
    parser.add_argument("scenario", choices=sorted(SCENARIOS))
    args = parser.parse_args()
    SCENARIOS[args.scenario](args)


if __name__ == "__main__":
    main()
