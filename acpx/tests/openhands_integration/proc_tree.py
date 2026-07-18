"""Pure-stdlib process-tree helpers.

Used to prove the acpx <-> OpenHands integration drives a *real* process
lifecycle -- an actual `acpx-server` process, with an actual
`claude-agent-acp`/`codex-acp` adapter process underneath it, both
parented (transitively) by the already-running OpenHands agent-server --
rather than just trusting the HTTP-level "the agent replied" signal as a
black box. See `README.md`'s "why process-tree assertions" section.

No `psutil` (or any other third-party) dependency on purpose: this
package's whole point is to be runnable via a bare
``uv run --with openhands-sdk==<version> --with pytest`` invocation (see
`README.md`) without pinning yet another package version against the
OpenHands SDK's own dependency set.
"""

from __future__ import annotations

import os
import subprocess
import time
from dataclasses import dataclass


@dataclass(frozen=True)
class ProcInfo:
    pid: int
    ppid: int
    cmd: str


def snapshot() -> list[ProcInfo]:
    """One-shot snapshot of every process on the host, via `ps -eo
    pid,ppid,args`. Linux-only (matches the rest of this workspace's
    tooling -- `acpx`'s own Rust integration tests assume a POSIX host
    too, see e.g. `binary_self_test.rs`'s use of `Stdio`)."""
    output = subprocess.run(
        ["ps", "-eo", "pid,ppid,args"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    procs: list[ProcInfo] = []
    for line in output.splitlines()[1:]:  # skip the header row
        line = line.strip()
        if not line:
            continue
        pid_str, ppid_str, cmd = line.split(maxsplit=2)
        try:
            procs.append(ProcInfo(pid=int(pid_str), ppid=int(ppid_str), cmd=cmd))
        except ValueError:
            continue
    return procs


def snapshot_stable(*, attempts: int = 5, delay: float = 0.2) -> list[ProcInfo]:
    """`snapshot()`, retried up to `attempts` times until two consecutive
    reads agree on the total process count.

    A single `ps -eo` snapshot has been observed (in this workspace's own
    sandboxed test-execution environment -- see `README.md`'s "a note on
    `ps` snapshot stability") to occasionally omit a handful of real,
    still-running processes with no corresponding exit -- i.e. a bare
    `subprocess.run(["ps", ...])` isn't always a fully-consistent read of
    `/proc`, independent of anything this suite itself does. Comparing
    two consecutive reads' *counts* (cheap, no need to diff full process
    lists) is enough to detect "that read was probably incomplete" and
    retry rather than trusting the first snapshot blindly -- this is what
    every `proc_tree` caller that matters for a test *assertion* (as
    opposed to a one-off debug print) should use instead of `snapshot()`
    directly.
    """
    previous_count: int | None = None
    stable: list[ProcInfo] | None = None
    for _ in range(max(1, attempts)):
        current = snapshot()
        if previous_count is not None and len(current) == previous_count:
            return current
        previous_count = len(current)
        stable = current
        time.sleep(delay)
    # Ran out of attempts without ever seeing two agreeing reads in a row
    # -- return the last one anyway (better than raising) rather than
    # blocking a caller indefinitely; a caller doing a bounded retry loop
    # of its own (e.g. the pytest suite's process-tree assertion) still
    # gets another chance on its own next iteration either way.
    assert stable is not None
    return stable


def descendants_matching(root_pid: int, *substrings: str) -> list[ProcInfo]:
    """Every process transitively parented by `root_pid` whose command
    line contains at least one of `substrings` -- e.g.
    `descendants_matching(agent_server_pid, "acpx-server")` to confirm a
    real `acpx-server` process is (still) running somewhere under the
    OpenHands agent-server's own process tree.

    A conversation's real process chain is
    ``agent-server -> acpx-server (wrapper script's exec target)
    -> claude-agent-acp/codex-acp -> <the real CLI binary>`` -- several
    levels deep, so this walks the full transitive descendant set rather
    than only direct children.
    """
    procs = snapshot_stable()
    by_parent: dict[int, list[ProcInfo]] = {}
    for proc in procs:
        by_parent.setdefault(proc.ppid, []).append(proc)

    matched: list[ProcInfo] = []
    frontier = [root_pid]
    seen: set[int] = set()
    while frontier:
        pid = frontier.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for child in by_parent.get(pid, []):
            frontier.append(child.pid)
            if any(s in child.cmd for s in substrings):
                matched.append(child)
    return matched


def find_pid_by_cmd_substring(substring: str) -> int | None:
    """First pid (lowest, i.e. most-likely-the-original-launch) whose
    command line contains `substring` -- used to locate the already-
    running OpenHands agent-server process itself (see
    `openhands_sdk_driver.discover_agent_server_pid`), since this test
    suite attaches to an operator-launched agent-server rather than
    spawning its own (see `README.md`'s "assumes a running OpenHands
    stack" section for why)."""
    matches = sorted(p.pid for p in snapshot_stable() if substring in p.cmd)
    return matches[0] if matches else None


def find_agent_server_pid(
    *, attempts: int = 10, delay: float = 0.2
) -> int | None:
    """Locate an OpenHands ``agent-server`` console script.

    The normal ``uvx`` launch has ``agent-server --host`` in its command
    line, but an installed Python console script appears as
    ``.../bin/python .../bin/agent-server --host``. Match the executable's
    basename instead of requiring the bare command spelling so lifecycle
    checks work in both supported launch forms.
    """
    for _ in range(max(1, attempts)):
        matches: list[int] = []
        for proc in snapshot_stable():
            words = proc.cmd.split()
            if "--host" not in words:
                continue
            if any(os.path.basename(word) == "agent-server" for word in words):
                matches.append(proc.pid)
        if matches:
            return min(matches)
        time.sleep(delay)
    return None
