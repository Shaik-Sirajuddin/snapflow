"""Pure-stdlib process-tree helpers for the black-box recovery suite.

`recovery_transport_helpers` (`acpx-session-recovery` plan): the phased
plan names this exact path as a "process-tree assertions proving
expected connector reuse/restart and absence of leaks" asset. This
suite's own scenarios spawn a real `acpx-server` binary that in turn
spawns real (stand-in shell script) backend connector processes -- an
HTTP-level "the daemon replied" assertion alone cannot distinguish a
genuinely-restarted connector process from a leaked one still running
under the old pid, so tests that care about that distinction snapshot
the real OS process tree instead.

Deliberately no third-party dependency (unlike `acp_ws_client.py`,
which has no stdlib alternative for speaking WebSocket at all) -- `ps`
is available on every POSIX host this workspace's own tooling already
assumes (see e.g. `acpx-server/tests/binary_self_test.rs`'s use of
`Stdio`), so there is no reason to require one here. Mirrors
`tests/openhands_integration/proc_tree.py`'s generic
snapshot/descendants shape (kept as an independent copy rather than a
cross-package import: the two suites are invoked completely separately
-- see each package's own `README`/module doc comment -- and neither
should have to import the other just to run standalone).
"""

from __future__ import annotations

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
    pid,ppid,args`."""
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
    """`snapshot()`, retried until two consecutive reads agree on the
    total process count -- see `tests/openhands_integration/
    proc_tree.py`'s identically-named function for why a single `ps`
    read is not always trustworthy in this workspace's own sandboxed
    test-execution environment."""
    previous_count: int | None = None
    stable: list[ProcInfo] | None = None
    for _ in range(max(1, attempts)):
        current = snapshot()
        if previous_count is not None and len(current) == previous_count:
            return current
        previous_count = len(current)
        stable = current
        time.sleep(delay)
    assert stable is not None
    return stable


def descendants_matching(root_pid: int, *substrings: str) -> list[ProcInfo]:
    """Every process transitively parented by `root_pid` whose command
    line contains at least one of `substrings` -- e.g.
    `descendants_matching(server_pid, "backend.sh")` to find this
    suite's own stand-in connector process(es) under a given
    `acpx-server` instance."""
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


def wait_until_gone(pids: list[int], *, timeout: float = 5.0, delay: float = 0.1) -> list[int]:
    """Polls until none of `pids` appear in a live snapshot anymore (a
    real backend process actually exited, not just "acpx-server closed
    its own handle to it"), or `timeout` elapses. Returns whichever
    `pids` are still alive when it gives up -- an empty list means every
    pid genuinely exited, which is what a leak-freedom assertion should
    check for."""
    deadline = time.monotonic() + timeout
    remaining = set(pids)
    while remaining and time.monotonic() < deadline:
        alive = {proc.pid for proc in snapshot()}
        remaining &= alive
        if remaining:
            time.sleep(delay)
    return sorted(remaining)
