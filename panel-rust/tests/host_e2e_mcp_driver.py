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
        return json.loads(text)

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
        matches = find_elements_by_accessible_label(
            client, root_handle, label, max_elements
        )
        if matches:
            return matches[0]["handle"]
        time.sleep(0.2)
    raise RuntimeError(f"no element with accessibleLabel={label!r} appeared in time")


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
    return [
        json.loads(line)
        for line in event_log.read_text().splitlines()
        if line.strip()
    ]


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


SCENARIOS = {
    "send-now": scenario_send_now,
    "rename": scenario_rename,
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mcp-url", default="http://127.0.0.1:18999/mcp")
    parser.add_argument("--event-log", type=pathlib.Path, required=True)
    parser.add_argument("--host-log", type=pathlib.Path)
    parser.add_argument("--timeout", type=float, default=15)
    parser.add_argument("scenario", choices=sorted(SCENARIOS))
    args = parser.parse_args()
    SCENARIOS[args.scenario](args)


if __name__ == "__main__":
    main()
