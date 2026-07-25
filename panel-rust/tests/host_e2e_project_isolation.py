#!/usr/bin/env python3
"""PISO-6: live e2e umbrella for the project-isolation-mlt-binding plan.

Drives a REAL Shotcut/Snapflow process (real acpx-server, real
rui-mock-agent, real sqlite) through the full matrix the plan's PISO-6
entry names: open project A -> threads scoped to A + real acpx cwd is A;
switch to project B -> distinct threads + B's cwd; restart -> durable
association survives; Save-As -> threads follow (PISO-7); close -> panel
clears.

Two input mechanisms, combined (neither alone reaches the whole matrix):
- `host_e2e_mcp_driver`'s MCP client, for everything inside the Slint
  panel (compose/send, thread list, rename) -- reused, not reimplemented.
- Raw XTEST (via `host_e2e_driver`'s primitives) for the surrounding QT
  chrome the MCP testing backend cannot see at all: Ctrl+W (Close),
  Ctrl+Shift+S (Save As) and typing into the resulting native-style
  QFileDialog. There is no MCP-reachable substitute for either.
- Project open/switch use Snapflow's own `snapflow-file-open` QLocalServer
  IPC (see main.cpp) instead of a File>Open dialog -- the exact mechanism
  a real OS file-open event uses, and avoids a second fragile dialog.

Ground truth for assertions is read directly, not screen-scraped, wherever
a screen-scrape would be indirect evidence of the same thing a file already
answers directly:
- Live, in-memory scoping (whether a thread is currently VISIBLE) can only
  be answered by the panel's own UI tree, so those checks do go through
  MCP `get_element_tree` (the sidebar row's own `button-accessible-label`
  is `"Rename thread " + thread.name`, so scanning for that prefix and
  stripping it is a robust proxy for "the currently visible thread
  names" -- see `sidebar_thread_row.slint`).
- Durable per-thread project association is read straight from
  `thread_settings.project_path` in `panel-state.sqlite3` (`state_store.
  rs`) -- the exact table PISO-3 added.
- Real acpx session cwd is read from `rui-mock-agent`'s own event log:
  this driver adds `request.cwd` as `session/new`'s `detail` field
  specifically for this test (mock_agent.rs), correlated to a thread via
  the `session_id` on that thread's own `session/prompt` event.
"""

import argparse
import json
import pathlib
import shutil
import socket
import subprocess
import sys
import time

sys.path.insert(0, str(pathlib.Path(__file__).parent))

import host_e2e_mcp_driver as mcp  # noqa: E402
from Xlib import X, XK, display  # noqa: E402
from Xlib.ext import xtest  # noqa: E402

# This file lives in THIS worktree (worktree-project-isolation), three
# levels below its root -- .../project-isolation/panel-rust/tests/<file>.
WORKTREE_ROOT = pathlib.Path(__file__).resolve().parents[2]
# acpx-server and the checked-out shotcut-rebrand repo (with the
# TEMPORARY CMakeLists repoint already built against WORKTREE_ROOT's own
# panel-rust) live in the main checkout, four levels above the worktree
# root: .../multi_media_main/.claude/worktrees/project-isolation.
MAIN_CHECKOUT_ROOT = WORKTREE_ROOT.parents[2]


class Failure(RuntimeError):
    pass


def log(msg):
    print(f"[piso6] {msg}", flush=True)


# One-off debugging aid for the Save-As XTEST sequence -- PISO6_DEBUG_DIR
# unset is a complete no-op (no scrot dependency in the normal run path).
def _debug_shot(label):
    import os

    debug_dir = os.environ.get("PISO6_DEBUG_DIR")
    if not debug_dir:
        return
    subprocess.run(
        ["scrot", f"{debug_dir}/{label}.png"],
        env={"DISPLAY": os.environ.get("DISPLAY", ":0")},
    )


# ---------------------------------------------------------------------------
# XTEST helpers (Ctrl+W / Ctrl+Shift+S / typing into the native file dialog).
# host_e2e_driver.py has `keycode`/`tap`/`type_text` but no modifier support
# -- both shortcuts this scenario needs are modified, so that's added here
# rather than widening the shared XTEST driver for a two-scenario need.
# ---------------------------------------------------------------------------


def chord(xdisplay, mod_chars, char):
    mod_codes = [xdisplay.keysym_to_keycode(XK.string_to_keysym(m)) for m in mod_chars]
    code = xdisplay.keysym_to_keycode(XK.string_to_keysym(char))
    for m in mod_codes:
        xtest.fake_input(xdisplay, X.KeyPress, m)
    xtest.fake_input(xdisplay, X.KeyPress, code)
    xtest.fake_input(xdisplay, X.KeyRelease, code)
    for m in reversed(mod_codes):
        xtest.fake_input(xdisplay, X.KeyRelease, m)
    xdisplay.sync()
    time.sleep(0.2)


# host_e2e_driver.type_text's keycode() maps a single char straight to an
# X keysym NAME via XK.string_to_keysym -- true for every letter/digit
# (its own name) and space (special-cased there), but punctuation needs
# the keysym's actual name, not the literal character. Every existing
# caller only ever typed thread names (letters/digits/spaces), so this
# never came up before a real filesystem PATH needed typing into the
# Save-As dialog. Kept local rather than widening the shared driver for
# a one-scenario need.
_PATH_KEYSYM_NAMES = {
    "/": "slash",
    ".": "period",
    "-": "minus",
    "_": "underscore",
    ":": "colon",
}


def type_path(xdisplay, text):
    from host_e2e_driver import keycode, tap

    for char in text:
        name = _PATH_KEYSYM_NAMES.get(char, char)
        keysym = XK.XK_space if char == " " else XK.string_to_keysym(name)
        if keysym == 0:
            raise RuntimeError(f"no X keysym for path char {char!r}")
        tap(xdisplay, xdisplay.keysym_to_keycode(keysym))


def select_all_and_type(xdisplay, text):
    chord(xdisplay, ["Control_L"], "a")
    time.sleep(0.1)
    type_path(xdisplay, text)


def press_enter(xdisplay):
    from host_e2e_driver import keycode, tap

    tap(xdisplay, keycode(xdisplay, "Return"))


# Ground-truthed once via a manual scrot screenshot against this exact
# 1280x800 Xvfb screen/window size: QFileDialog does NOT default focus to
# the "File name:" field here (it lands on the file-list view instead),
# so Ctrl+A without first clicking the field selects files in the list,
# not text in the field -- typing after that goes nowhere useful. This is
# the field's own on-screen center at that fixed geometry.
SAVE_DIALOG_FILENAME_FIELD_XY = (620, 516)


def save_project_as(xdisplay, target_path: pathlib.Path):
    """Ctrl+Shift+S, clicks the file dialog's "File name:" field (see
    SAVE_DIALOG_FILENAME_FIELD_XY), types the absolute path, and confirms.

    Path must be lowercase-only (besides digits/punctuation): XTEST key
    taps here send a bare keycode press with no Shift held, so a keysym
    named after an uppercase letter still produces that key's unshifted
    (lowercase) glyph -- ground-truthed the same way as the field's
    coordinates above. Real Shift-chording was not worth building for a
    one-scenario need; every path this module constructs is deliberately
    lowercase instead.
    """
    from host_e2e_driver import click

    # There is no window manager on this Xvfb display to assign input
    # focus on map, so XTEST-delivered keys can go nowhere if nothing has
    # ever been clicked yet -- ground-truthed live: the very first
    # Ctrl+Shift+S right after MCP comes up landed on an unfocused
    # window and produced no dialog at all (reproduced 2/2 in the full
    # script's own timing; a manual repro with several seconds of
    # incidental delay between launch and the chord never hit this). A
    # menu-bar click is a stable, state-independent way to force focus
    # onto MainWindow before every Save-As, not just the first one.
    click(xdisplay, 640, 8)
    time.sleep(0.2)
    _debug_shot("0_before_chord")
    chord(xdisplay, ["Control_L", "Shift_L"], "s")
    time.sleep(1.0)
    _debug_shot("1_after_chord")

    click(xdisplay, *SAVE_DIALOG_FILENAME_FIELD_XY)
    _debug_shot("2_after_field_click")
    select_all_and_type(xdisplay, str(target_path))
    time.sleep(0.3)
    _debug_shot("3_after_type")
    press_enter(xdisplay)
    time.sleep(0.5)
    _debug_shot("4_after_enter")
    # First save to a NEW filename never triggers Shotcut's own overwrite
    # prompt (the file doesn't exist yet); every path this scenario saves
    # to is unique, so no second confirmation is expected here.


def close_project(xdisplay):
    from host_e2e_driver import click

    # Same no-window-manager focus concern as save_project_as's own click.
    click(xdisplay, 640, 8)
    time.sleep(0.2)
    chord(xdisplay, ["Control_L"], "w")
    time.sleep(0.5)


# ---------------------------------------------------------------------------
# snapflow-file-open IPC: same code path a real OS file-open event drives
# (main.cpp's QLocalServer -> MainWindow::openMultiple). Used here for
# project open/switch instead of automating a File>Open dialog.
# ---------------------------------------------------------------------------


def send_file_open(path: pathlib.Path, timeout=10):
    deadline = time.monotonic() + timeout
    last_error = None
    while time.monotonic() < deadline:
        try:
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            sock.connect(f"\0snapflow-file-open")
            sock.sendall(str(path).encode("utf-8"))
            sock.close()
            return
        except OSError as exc:
            last_error = exc
            # Fall back to the filesystem-namespace path QLocalServer uses
            # when the abstract namespace isn't available/attempted.
            try:
                sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                sock.connect("/tmp/snapflow-file-open")
                sock.sendall(str(path).encode("utf-8"))
                sock.close()
                return
            except OSError as exc2:
                last_error = exc2
            time.sleep(0.2)
    raise Failure(f"could not reach snapflow-file-open IPC socket: {last_error}")


# ---------------------------------------------------------------------------
# Panel-state readers: sqlite (durable) + mock-agent event log (real cwd).
# ---------------------------------------------------------------------------


def thread_project_path(db_path: pathlib.Path, display_name: str, timeout=10):
    import sqlite3

    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if db_path.exists():
            try:
                conn = sqlite3.connect(str(db_path))
                row = conn.execute(
                    "SELECT project_path FROM thread_settings WHERE display_name = ? "
                    "ORDER BY rowid DESC LIMIT 1",
                    (display_name,),
                ).fetchone()
                conn.close()
                if row is not None:
                    return row[0]
            except sqlite3.OperationalError:
                pass
        time.sleep(0.3)
    raise Failure(f"thread_settings row for {display_name!r} never appeared in {db_path}")


def session_cwd_for_prompt(event_log: pathlib.Path, prompt_text: str, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        events = mcp.prompt_events(event_log)
        prompt = next(
            (e for e in events if e["method"] == "session/prompt" and e["detail"] == prompt_text),
            None,
        )
        if prompt is not None:
            new_session = next(
                (
                    e
                    for e in events
                    if e["method"] == "session/new" and e["session_id"] == prompt["session_id"]
                ),
                None,
            )
            if new_session is not None:
                return new_session["detail"]
        time.sleep(0.2)
    raise Failure(f"no session/new cwd found for prompt {prompt_text!r} in {event_log}")


def expand_thread_sidebar(client, root_handle, timeout=2):
    # "Rename thread <name>" labels (sidebar_thread_row.slint) and "New
    # thread" (sidebar.slint: `if expanded && !skill-mode`) only render
    # once the Threads rail is expanded -- it defaults collapsed (a 48px
    # icon rail). Not an error if it's already expanded (the label just
    # won't be there). Ground-truthed live: with the rail left collapsed
    # from an earlier collapse, visible_thread_names silently returned an
    # empty set instead of failing loudly -- a negative assertion against
    # an empty set is trivially true and proves nothing, so this must run
    # before every visibility check, not just before creating a thread.
    try:
        expand_handle = mcp.wait_for_accessible_label(
            client, root_handle, "Expand thread sidebar", timeout=timeout
        )
        mcp.click(client, expand_handle)
        time.sleep(0.3)
    except RuntimeError:
        pass


def visible_thread_names(client, root_handle, max_elements=800):
    # NOT "Rename thread <name>" (sidebar_thread_row.slint): that button
    # only renders `if ... (root.has-hover || index == selected-thread)`
    # -- real mouse hover never happens in this headless XTEST session,
    # so it only ever exists for the ONE currently-selected row, making
    # it useless for "which threads are in the filtered list" (a set of
    # at most 1, ALWAYS, no matter how many threads are actually
    # visible). Ground-truthed live: this returned a single-element set
    # for 15s straight through a real 6-thread restore. The row's OWN
    # `button-accessible-label: thread.name` (unconditional, on
    # HoverSurface itself, no hover/selection gate) is the real
    # unconditional-per-row signal.
    expand_thread_sidebar(client, root_handle)
    tree = client.call_tool(
        "get_element_tree", {"elementHandle": root_handle, "maxElements": max_elements}
    )
    # A row filtered out of the list still exists in the element tree --
    # sidebar_thread_row.slint's own comment: "Parent may override height
    # to 0 when the row is filtered out" -- so presence of a label alone
    # is not "visible"; a filtered row's own accessibleLabel is still
    # findable, just at zero size. Require a real on-screen footprint.
    labels = set()
    for element in tree.get("elements", []):
        label = element.get("accessibleLabel")
        if not label:
            continue
        size = element.get("size") or {}
        if (size.get("width") or 0) > 0 and (size.get("height") or 0) > 0:
            labels.add(label)
    if not labels:
        raise Failure(
            "visible_thread_names found zero labeled elements at all with "
            "the sidebar expanded -- either a real bug or this check "
            "itself is broken; either way a negative assertion against "
            "this result would prove nothing, so treat it as fatal."
        )
    return labels


def wait_for_visible_thread(client, root_handle, name, timeout=15):
    # Thread attachment after a restart is genuinely async and can take
    # several seconds per thread (ground-truthed live: shotcut-2's own
    # attachment log for a 6-thread restore spanned dozens of interleaved
    # log lines, with the LAST thread's attach arriving well after this
    # module's earlier fixed 1s settle). A single-shot visible_thread_
    # names call raced that and read a still-partial list. Polling is the
    # honest fix, not a longer fixed guess.
    deadline = time.monotonic() + timeout
    last_seen = set()
    while time.monotonic() < deadline:
        last_seen = visible_thread_names(client, root_handle)
        if name in last_seen:
            return
        time.sleep(0.5)
    raise Failure(f"{name!r} never became visible within {timeout}s; last saw {last_seen}")


def click_new_thread(client, root_handle, timeout=15):
    # Every default/cold-start thread already has an ACP session attached
    # from before any project was ever opened, so its session/new cwd was
    # captured back then -- ThreadSlot.project_path is captured once at
    # session creation and never updated after (see agent_bridge.rs's own
    # doc comment on that field), matching real ACP: there is no way to
    # move an existing session to a new cwd. Ground-truthed live: sending
    # on the pre-seeded default thread after opening a project reused its
    # OLD session and reported the OLD (pre-open) cwd, not project A's.
    # A thread created fresh AFTER the project is active is the only way
    # to observe that project's own cwd on session/new.
    # "New thread" only renders when the sidebar rail is expanded
    # (sidebar.slint: `if expanded && !skill-mode`) -- see
    # expand_thread_sidebar's own doc comment.
    expand_thread_sidebar(client, root_handle)
    new_thread_handle = mcp.wait_for_accessible_label(client, root_handle, "New thread", timeout=timeout)
    mcp.click(client, new_thread_handle)
    # The new thread's compose box isn't necessarily in the element tree
    # the instant the click callback returns (new-thread-requested's Msg
    # round-trip + a render frame) -- ground-truthed live.
    time.sleep(0.5)
    # Collapse back: the dock has a FIXED total width, and the expanded
    # Threads rail eats into it -- wide enough to squeeze ChatInputLayout
    # (and its "compose" element) down to where Slint doesn't render it
    # into the tree at all. Ground-truthed live: with the sidebar left
    # expanded, find_element_by_qualified_id("ChatInputLayout::compose")
    # came back "not found" even though the new thread was created and
    # selected correctly.
    try:
        collapse_handle = mcp.wait_for_accessible_label(
            client, root_handle, "Collapse thread sidebar", timeout=2
        )
        mcp.click(client, collapse_handle)
        time.sleep(0.3)
    except RuntimeError:
        pass


def send_and_wait(client, window_handle, event_log, text, timeout=15):
    compose_handle = mcp.find_element_by_qualified_id(
        client, window_handle, "ChatInputLayout::compose"
    )
    mcp.click(client, compose_handle)
    mcp.set_text(client, compose_handle, text)
    mcp.press_return(client, window_handle)
    mcp.wait_for_prompt_texts(event_log, [text], timeout=timeout)


def rename_active_thread(client, root_handle, window_handle, new_name, timeout=15):
    rename_handle = mcp.wait_for_accessible_label(client, root_handle, "Rename thread")
    mcp.click(client, rename_handle)
    name_input_handle = mcp.wait_for_accessible_label(client, root_handle, "Thread name")
    mcp.set_text(client, name_input_handle, new_name)
    mcp.press_return(client, window_handle)
    mcp.wait_for_accessible_label(client, root_handle, new_name, timeout=timeout)


# ---------------------------------------------------------------------------
# Process orchestration.
# ---------------------------------------------------------------------------


def wait_for_http(url, timeout=20):
    import urllib.request

    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            urllib.request.urlopen(url, timeout=1)
            return
        except Exception:
            time.sleep(0.2)
    raise Failure(f"{url} never came up")


def spawn_snapflow(env, state_dir, log_name, extra_args=None):
    shotcut_bin = env["SHOTCUT_BIN"]
    args = [shotcut_bin, "--appdata", str(state_dir / "shotcut"), "--noupgrade"]
    if extra_args:
        args += extra_args
    stdout = open(state_dir / f"{log_name}.stdout.log", "w")
    stderr = open(state_dir / f"{log_name}.stderr.log", "w")
    return subprocess.Popen(args, env=env, stdout=stdout, stderr=stderr)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--keep-state", action="store_true")
    parser.add_argument(
        "--state-dir", type=pathlib.Path, default=None, help="reuse an existing state dir"
    )
    args = parser.parse_args()

    if args.state_dir:
        state_dir = args.state_dir
    else:
        # NOT mktemp: its XXXXXX suffix is mixed-case, and every path this
        # module ever types into the real Save-As dialog is XTEST bare
        # keycode taps with no Shift held (see save_project_as's own doc
        # comment on why paths must be lowercase-only) -- a mixed-case
        # state_dir would get typed lowercase and silently miss the real
        # (differently-cased) directory, so Save-As would appear to do
        # nothing and the resulting Failure would have nothing to do with
        # the actual XTEST mechanics. Ground-truthed live: this was the
        # actual root cause of an earlier "Save-As did not produce ..."
        # failure in this exact harness. uuid4().hex is lowercase hex by
        # construction, so it can't reintroduce the same bug.
        import uuid

        state_dir = pathlib.Path(f"/tmp/panel-piso6.{uuid.uuid4().hex[:12]}")
        state_dir.mkdir(parents=True)
    (state_dir / "acpx").mkdir(parents=True, exist_ok=True)
    (state_dir / "panel").mkdir(parents=True, exist_ok=True)
    (state_dir / "shotcut").mkdir(parents=True, exist_ok=True)
    (state_dir / "projects").mkdir(parents=True, exist_ok=True)
    log(f"state dir = {state_dir}")

    display_name = ":97"
    gateway_port = 18999
    mcp_port = 19199
    db_path = state_dir / "panel" / "panel-state.sqlite3"
    event_log = state_dir / "acpx" / "backend-events.jsonl"

    server_bin = str(MAIN_CHECKOUT_ROOT / "acpx" / "target" / "debug" / "acpx-server")
    agent_bin = str(WORKTREE_ROOT / "panel-rust" / "target" / "debug" / "rui-mock-agent")
    shotcut_bin = str(
        MAIN_CHECKOUT_ROOT / "shotcut-rebrand" / "build-local" / "src" / "snapflow"
    )
    for binary in (server_bin, agent_bin, shotcut_bin):
        if not pathlib.Path(binary).is_file():
            raise Failure(f"required binary missing: {binary}")

    xvfb = subprocess.Popen(
        ["Xvfb", display_name, "-screen", "0", "1280x800x24", "-nolisten", "tcp"],
        stdout=open(state_dir / "xvfb.log", "w"),
        stderr=subprocess.STDOUT,
    )
    time.sleep(1.0)
    subprocess.run(
        ["xdpyinfo", "-display", display_name],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )

    fifo = state_dir / "acpx" / "stdin.fifo"
    import os

    os.mkfifo(fifo)
    fifo_fd = os.open(str(fifo), os.O_RDWR)

    server_env = dict(os.environ)
    server_env.update(
        {
            "ACPX_HTTP_BIND": f"127.0.0.1:{gateway_port}",
            "ACPX_BACKEND_CMD": agent_bin,
            "ACPX_DEFAULT_AGENT_ID": "codex",
            "ACPX_DB_PATH": str(state_dir / "acpx" / "gateway.sqlite3"),
            "RUI_MOCK_AGENT_EVENT_LOG": str(event_log),
        }
    )
    server = subprocess.Popen(
        [server_bin],
        env=server_env,
        stdin=fifo_fd,
        stdout=open(state_dir / "acpx" / "server.stdout.log", "w"),
        stderr=open(state_dir / "acpx" / "server.stderr.log", "w"),
    )

    results = {}
    panel_proc = None
    try:
        wait_for_http(f"http://127.0.0.1:{gateway_port}/health")

        env = dict(os.environ)
        env.update(
            {
                "DISPLAY": display_name,
                "SLINT_MCP_PORT": str(mcp_port),
                "RUI_PANEL_INPUT_TRACE": "1",
                "QSG_RENDER_LOOP": "basic",
                "RUI_ACP_CACHE_DIR": str(state_dir / "panel"),
                "RUI_ACPX_CODEX_URL": f"http://127.0.0.1:{gateway_port}",
                "RUI_ACPX_CLAUDE_URL": f"http://127.0.0.1:{gateway_port}",
                "SHOTCUT_BIN": shotcut_bin,
                # PISO-13: cold-start seeding now defaults to a single
                # empty "Chat" thread (unset/"0"), not the 4 named fixture
                # threads -- that default is intentionally no longer
                # fixture content. This harness doesn't assert on the
                # fixture names, but pins the old opt-in explicitly so a
                # thread already exists (and the default-thread/index
                # assumptions the rest of this script makes stay valid)
                # regardless of which way that default moves again later.
                "RUI_SEED_THREADS": "4",
                # Release builds fork a watchdog-parent/real-child pair
                # (main.cpp's kWatchdogEnvVar == "SNAPFLOW_WATCHDOG";
                # QT_DEBUG builds set this for themselves). This harness
                # wants exactly one process with a real MainWindow/MCP
                # server, not a supervisor plus a child it has to chase,
                # so it opts into what a debug build already does.
                "SNAPFLOW_WATCHDOG": "1",
            }
        )

        xdisplay = display.Display(display_name)

        def connect_mcp():
            client = mcp.McpClient(f"http://127.0.0.1:{mcp_port}/mcp")
            client.wait_until_up(timeout=20)
            return client, *mcp.get_root_element(client)

        # --- Launch fresh (untitled). -----------------------------------
        panel_proc = spawn_snapflow(env, state_dir, "shotcut-1")
        time.sleep(5)
        if panel_proc.poll() is not None:
            raise Failure("snapflow exited before MCP came up; see shotcut-1.stderr.log")
        client, window_handle, root_handle = connect_mcp()

        # --- Build A.mlt and B.mlt as real, independently-saved files. --
        project_a = state_dir / "projects" / "project-a.mlt"
        project_b = state_dir / "projects" / "project-b.mlt"
        save_project_as(xdisplay, project_a)
        if not project_a.exists():
            raise Failure(f"Save-As did not produce {project_a}")
        log("saved untitled -> A.mlt (first save, not a rename)")

        close_project(xdisplay)
        save_project_as(xdisplay, project_b)
        if not project_b.exists():
            raise Failure(f"Save-As did not produce {project_b}")
        log("saved untitled -> B.mlt (independent project, for the switch row)")
        close_project(xdisplay)

        # === Row 1: open A -> thread scoped to A, real acpx cwd is A. ===
        send_file_open(project_a)
        # openMultiple() runs async on the Qt event loop; the IPC write
        # returning has no bearing on when the panel's active_project_path
        # actually updates -- ground-truthed live (row1 raced this and read
        # the pre-open cwd). No ack exists on this channel, so a fixed
        # settle matches this script's style elsewhere.
        time.sleep(1.0)
        client, window_handle, root_handle = connect_mcp()
        click_new_thread(client, root_handle)
        send_and_wait(client, window_handle, event_log, "piso6 marker on A")
        rename_active_thread(client, root_handle, window_handle, "thread-on-A")
        cwd_a = session_cwd_for_prompt(event_log, "piso6 marker on A")
        if cwd_a != str(project_a):
            raise Failure(f"row1: acpx cwd {cwd_a!r} != project A {str(project_a)!r}")
        path_a = thread_project_path(db_path, "thread-on-A")
        if path_a != str(project_a):
            raise Failure(f"row1: thread_settings.project_path {path_a!r} != {str(project_a)!r}")
        results["row1_open_a_scoped_and_cwd"] = "PASS"
        log("PASS row1: thread-on-A scoped to A, real session/new cwd == A.mlt")

        # === Row 2: switch to B -> distinct threads, B's cwd. ===========
        send_file_open(project_b)
        time.sleep(1.0)  # see row1's own comment on this same race
        visible_after_switch = visible_thread_names(client, root_handle)
        if "thread-on-A" in visible_after_switch:
            raise Failure(
                f"row2: thread-on-A still visible after switching to B: {visible_after_switch}"
            )
        click_new_thread(client, root_handle)
        send_and_wait(client, window_handle, event_log, "piso6 marker on B")
        rename_active_thread(client, root_handle, window_handle, "thread-on-B")
        cwd_b = session_cwd_for_prompt(event_log, "piso6 marker on B")
        if cwd_b != str(project_b):
            raise Failure(f"row2: acpx cwd {cwd_b!r} != project B {str(project_b)!r}")
        results["row2_switch_b_distinct_and_cwd"] = "PASS"
        log("PASS row2: switching to B hides thread-on-A, thread-on-B gets B's real cwd")

        # === Row 3: restart -> durable association survives. ============
        panel_proc.terminate()
        try:
            panel_proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            # SIGTERM alone isn't reliably enough to end this process in
            # this harness -- ground-truthed live. A hard kill is fine
            # here: row3 only cares that sqlite already has the durable
            # association (written before this point), not a graceful
            # shutdown.
            panel_proc.kill()
            panel_proc.wait(timeout=10)
        panel_proc = spawn_snapflow(env, state_dir, "shotcut-2", extra_args=[str(project_a)])
        time.sleep(5)
        if panel_proc.poll() is not None:
            raise Failure("snapflow (restart) exited before MCP came up; see shotcut-2.stderr.log")
        client, window_handle, root_handle = connect_mcp()
        wait_for_visible_thread(client, root_handle, "thread-on-A")
        path_a_after_restart = thread_project_path(db_path, "thread-on-A")
        if path_a_after_restart != str(project_a):
            raise Failure(
                f"row3: project_path after restart {path_a_after_restart!r} != {str(project_a)!r}"
            )
        results["row3_restart_durable"] = "PASS"
        log("PASS row3: thread-on-A's association survived a real process restart")

        # === Row 4 (PISO-7): Save-As -> threads follow, live, no restart.
        project_a_renamed = state_dir / "projects" / "project-a-renamed.mlt"
        save_project_as(xdisplay, project_a_renamed)
        if not project_a_renamed.exists():
            raise Failure(f"Save-As did not produce {project_a_renamed}")
        wait_for_visible_thread(client, root_handle, "thread-on-A")
        path_a_after_rename = thread_project_path(db_path, "thread-on-A")
        if path_a_after_rename != str(project_a_renamed):
            raise Failure(
                f"row4: project_path after Save-As {path_a_after_rename!r} != "
                f"{str(project_a_renamed)!r}"
            )
        results["row4_save_as_follows_live"] = "PASS"
        log("PASS row4 (PISO-7): thread-on-A followed Save-As live, no restart needed")

        # === Row 5: close -> panel clears. ===============================
        # "Panel clears" means the ACTIVE PROJECT association clears, not
        # that previously-visible threads vanish from the sidebar --
        # ground-truthed against retain_items_for_project itself
        # (models.rs): `let Some(active) = active_project_path.filter(...)
        # else { return; };` returns WITHOUT filtering at all when there
        # is no active project, by design -- nothing to scope AGAINST, so
        # every thread stays visible. This is deliberate, not a bug: an
        # earlier version of this row asserted thread-on-A disappears
        # after Close, which contradicts this code on purpose and was
        # simply the wrong invariant to test. The real, provable claim is
        # that a thread created AFTER Close is unscoped (gets the process
        # cwd fallback, not any project's path) -- the same signal row1/
        # row2 already use to prove the opposite (a thread IS scoped).
        close_project(xdisplay)
        time.sleep(1.0)
        click_new_thread(client, root_handle)
        send_and_wait(client, window_handle, event_log, "piso6 marker after close")
        cwd_after_close = session_cwd_for_prompt(event_log, "piso6 marker after close")
        if cwd_after_close in (str(project_a), str(project_b), str(project_a_renamed)):
            raise Failure(
                f"row5: new thread after Close still scoped to a project: {cwd_after_close!r}"
            )
        results["row5_close_clears"] = "PASS"
        log("PASS row5: Close clears the active project -- a new thread is unscoped again")

    finally:
        if panel_proc is not None and panel_proc.poll() is None:
            panel_proc.terminate()
            try:
                panel_proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                panel_proc.kill()
        if server.poll() is None:
            server.terminate()
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server.kill()
        os.close(fifo_fd)
        xvfb.terminate()
        try:
            xvfb.wait(timeout=5)
        except subprocess.TimeoutExpired:
            xvfb.kill()
        if not args.keep_state:
            shutil.rmtree(state_dir, ignore_errors=True)
        else:
            log(f"state retained at {state_dir}")

    print(json.dumps(results, indent=2))
    if len(results) < 5:
        print("PISO-6: PARTIAL -- not every row proved, see output above", file=sys.stderr)
        sys.exit(1)
    print("PISO-6: all 5 matrix rows PASS")


if __name__ == "__main__":
    main()
