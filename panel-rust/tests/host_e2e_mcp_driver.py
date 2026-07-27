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
    window_handle = handles[0]
    props = client.call_tool("get_window_properties", {"windowHandle": window_handle})
    return window_handle, props.get("rootElementHandle") or {}


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
    raise RuntimeError(f"no element with accessibleLabel={label!r} appeared in time")


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


SCENARIOS = {
    "send-now": scenario_send_now,
    "fast-track": scenario_fast_track,
    "rename": scenario_rename,
    "startup-warning": scenario_startup_warning,
    "mid-session-write-failure": scenario_mid_session_write_failure,
    "real-agent-smoke": scenario_real_agent_smoke,
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
