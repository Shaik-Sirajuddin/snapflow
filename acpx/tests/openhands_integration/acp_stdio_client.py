"""Minimal async ACP-over-stdio client, used to drive a real `acpx-server`
subprocess exactly the way an ACP client (OpenHands's `ACPAgent`, an
editor, `rui-acpx-client`, ...) does: spawn it, write newline-delimited
JSON-RPC requests to its stdin, read newline-delimited JSON-RPC frames
(responses interleaved with `session/update` notifications) off its
stdout.

This replaces the earlier practice of hand-typing `initialize`/
`session/new`/`session/prompt` JSON one request at a time into a raw PTY
session -- that was fine for a one-off manual probe, but is not
reproducible, not diffable, and not something CI (or a future review
pass) can re-run. Everything this file's manual predecessor did by hand
is captured here as a small, reusable, type-hinted client.

Deliberately dependency-light: only the Python standard library
(`asyncio`, `json`, `dataclasses`) -- no `pytest`-only or third-party
ACP/JSON-RPC package, so this same client can be imported by a plain
`python3 -m` smoke-test script (see `manual_smoke_test.py`) as easily as
by the `pytest` suite (`test_claude_backend.py`/`test_codex_backend.py`).
"""

from __future__ import annotations

import asyncio
import itertools
import json
import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, AsyncIterator


class AcpRpcError(RuntimeError):
    """Raised when a request's JSON-RPC response carries an `error` member.

    Mirrors the shape `acpx-core::router::RouterError` responses take on
    the wire (`{"code": ..., "message": ..., "data": ...}`) so a caller
    can match on `error.code`/`error.message` without re-parsing the raw
    dict every time.
    """

    def __init__(self, method: str, error: dict[str, Any]) -> None:
        self.method = method
        self.error = error
        code = error.get("code")
        message = error.get("message")
        super().__init__(f"{method}: JSON-RPC error {code}: {message}")


@dataclass
class AcpStdioClient:
    """Owns one spawned `acpx-server` child process and its stdio framing.

    Construct via `AcpStdioClient.spawn(...)` (an async classmethod, since
    spawning a subprocess is inherently async under `asyncio`); use as an
    async context manager so the child is always terminated, even if a
    test assertion raises mid-conversation:

        async with await AcpStdioClient.spawn(wrapper_script) as client:
            await client.initialize()
            session_id = await client.session_new(cwd="/tmp")
            reply = await client.prompt_text(session_id, "hello")
    """

    process: asyncio.subprocess.Process
    stderr_task: asyncio.Task[bytes]
    _next_id: itertools.count = field(default_factory=lambda: itertools.count(1))
    _pending_notifications: list[dict[str, Any]] = field(default_factory=list)

    @classmethod
    async def spawn(
        cls,
        command: Path | str,
        *args: str,
        env_overrides: dict[str, str] | None = None,
    ) -> "AcpStdioClient":
        """Spawn `command` (e.g. one of `scripts/openhands-acpx-*.sh`) with
        its stdin/stdout/stderr all piped, ready for `initialize()` to be
        the first call -- per the ACP spec, and per this repo's own
        `binary_self_test.rs`'s doc comment on the same requirement.

        `env_overrides` merges onto (does not replace) the current
        process's environment -- lets a caller point `ACPX_SERVER_BIN`/
        `ACPX_BACKEND_CMD` at something other than the wrapper script's
        own default without needing a second wrapper script per variant.
        """
        env = os.environ.copy()
        if env_overrides:
            env.update(env_overrides)
        process = await asyncio.create_subprocess_exec(
            str(command),
            *args,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=env,
        )
        assert process.stderr is not None
        # Drain stderr continuously into a buffer rather than leaving it
        # unread -- an unread pipe fills its OS buffer and can deadlock
        # the child once it tries to log enough; captured so a failing
        # test can still print it for diagnosis (see `stderr_text()`).
        stderr_task = asyncio.ensure_future(process.stderr.read())
        return cls(process=process, stderr_task=stderr_task)

    async def __aenter__(self) -> "AcpStdioClient":
        return self

    async def __aexit__(self, *exc_info: object) -> None:
        await self.close()

    async def close(self) -> None:
        """Terminate the child process (if still running) and reap it,
        same as `ServerGuard`'s `Drop` impl does for the Rust integration
        tests this mirrors."""
        if self.process.returncode is None:
            self.process.kill()
        try:
            await asyncio.wait_for(self.process.wait(), timeout=5)
        except asyncio.TimeoutError:
            pass
        if not self.stderr_task.done():
            self.stderr_task.cancel()

    async def stderr_text(self) -> str:
        """Best-effort decode of everything captured off the child's
        stderr so far -- useful in a pytest failure message, mirroring
        why `real_binary_survives_a_concurrent_bind_conflict_and_still_
        serves_stdio` (the Rust equivalent of this file) asserts on
        stderr content directly."""
        if self.stderr_task.done():
            raw = self.stderr_task.result()
        else:
            raw = b""
        return raw.decode("utf-8", errors="replace")

    async def _write(self, request: dict[str, Any]) -> None:
        assert self.process.stdin is not None
        line = json.dumps(request) + "\n"
        self.process.stdin.write(line.encode("utf-8"))
        await self.process.stdin.drain()

    async def _read_frame(self, *, timeout: float) -> dict[str, Any]:
        assert self.process.stdout is not None
        raw = await asyncio.wait_for(self.process.stdout.readline(), timeout=timeout)
        if not raw:
            stderr = await self.stderr_text()
            raise RuntimeError(
                f"acpx-server stdout closed unexpectedly (process exited? "
                f"returncode={self.process.returncode}); stderr so far:\n{stderr}"
            )
        return json.loads(raw)

    async def call(
        self, method: str, params: dict[str, Any] | None = None, *, timeout: float = 30.0
    ) -> dict[str, Any]:
        """Send one JSON-RPC request and return its `result`, buffering
        any interleaved `session/update` notifications (frames with a
        `method` but no matching `id`) into `notifications_since_last_call`
        along the way -- mirrors `acpx-core::router::read_matching_response`'s
        own "collect every unmatched message seen along the way" behavior
        on the *client* side of the same wire protocol.

        Raises `AcpRpcError` if the response carries an `error` member.
        """
        request_id = next(self._next_id)
        request: dict[str, Any] = {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params or {},
        }
        await self._write(request)
        while True:
            frame = await self._read_frame(timeout=timeout)
            if frame.get("id") == request_id:
                if "error" in frame:
                    raise AcpRpcError(method, frame["error"])
                return frame.get("result", {})
            # Any other id-less or different-id frame is a notification
            # (almost always `session/update`) that arrived while this
            # call was in flight -- buffered, not dropped, same as the
            # gateway's own `_acpx.updates` semantics for a client with no
            # live push channel.
            self._pending_notifications.append(frame)

    def drain_notifications(self) -> list[dict[str, Any]]:
        """Pop and return every notification buffered by `call()` so far."""
        drained = self._pending_notifications
        self._pending_notifications = []
        return drained

    async def notifications(self) -> AsyncIterator[dict[str, Any]]:
        """Yield already-buffered notifications first, then keep reading
        raw frames off stdout as they arrive (blocking between each) --
        an async generator a caller can `async for` over when it wants to
        observe live `agent_message_chunk`/`usage_update` traffic during a
        long-running `prompt()` call from a second concurrent task."""
        for frame in self.drain_notifications():
            yield frame
        while True:
            yield await self._read_frame(timeout=120.0)

    # -- Convenience wrappers over the raw ACP methods actually exercised
    #    by this integration (see `acpx/README.md`'s "Configuration"
    #    section and `acpx-core/src/router.rs`'s classification table for
    #    the full method surface; this is deliberately just the subset an
    #    OpenHands-shaped client -- one `initialize`, one `session/new`,
    #    N `session/prompt` calls -- actually needs). --

    async def initialize(self, *, protocol_version: int = 1) -> dict[str, Any]:
        return await self.call(
            "initialize",
            {"protocolVersion": protocol_version, "clientCapabilities": {}},
        )

    async def session_new(self, *, cwd: str, mcp_servers: list[Any] | None = None) -> str:
        """`session/new`; returns the gateway-minted session id (never the
        backend's own raw one -- see `AcpStdioClient`'s module doc
        comment and `router.rs`'s session-id-translation doc comments for
        why the two must never be conflated)."""
        result = await self.call(
            "session/new", {"cwd": cwd, "mcpServers": mcp_servers or []}
        )
        session_id = result.get("sessionId")
        if not isinstance(session_id, str):
            raise RuntimeError(f"session/new response missing sessionId: {result}")
        return session_id

    async def prompt_text(
        self, session_id: str, text: str, *, timeout: float = 60.0
    ) -> str:
        """`session/prompt` with a single text block; returns the
        concatenated `agent_message_chunk` text observed for this call
        (mirrors `acpx-client::ext::prompt`'s own chunk-concatenation
        helper on the Rust client SDK side, kept independent here rather
        than imported so this test harness has zero dependency on
        `acpx-client`'s own crate build)."""
        request_id = next(self._next_id)
        request = {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": text}],
            },
        }
        await self._write(request)
        chunks: list[str] = []
        while True:
            frame = await self._read_frame(timeout=timeout)
            if frame.get("id") == request_id:
                if "error" in frame:
                    raise AcpRpcError("session/prompt", frame["error"])
                break
            update = frame.get("params", {}).get("update", {})
            if update.get("sessionUpdate") == "agent_message_chunk":
                content = update.get("content", {})
                if content.get("type") == "text":
                    chunks.append(content.get("text", ""))
            else:
                self._pending_notifications.append(frame)
        return "".join(chunks)

    async def authenticate(self, method_id: str) -> dict[str, Any]:
        return await self.call("authenticate", {"methodId": method_id})

    async def session_close(self, session_id: str) -> dict[str, Any]:
        return await self.call("session/close", {"sessionId": session_id})
