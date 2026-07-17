"""Minimal WebSocket client for ACPX `/ws` recovery tests.

`recovery_transport_helpers` (`acpx-session-recovery` plan): the phased
plan names this exact path as the black-box suite's WebSocket
counterpart to `acp_http_client.py` -- request/response round trips over
`/rpc` alone cannot exercise reconnect (`_acpx.resume`) or live
`session/update` push, both of which only exist on the persistent
transports (`/ws`, stdio). See `acpx-server/src/transport/live.rs` and
`acpx_core::notify` for the server-side halves of the `_acpx.resume`/
`_acpx.seq`/`_acpx.epoch` contract this client speaks.

Depends on the third-party `websockets` package (not stdlib, unlike
`acp_http_client.py`) -- there is no dependency-free way to speak the
WebSocket protocol from CPython's standard library alone. Install with
`pip install websockets` (or `uv run --with websockets`) before running
any test that imports this module.
"""

from __future__ import annotations

import asyncio
import itertools
import json
from dataclasses import dataclass, field
from typing import Any

import websockets


class JsonRpcError(RuntimeError):
    """A JSON-RPC error response returned by the ACPX daemon."""

    def __init__(self, method: str, error: dict[str, Any]) -> None:
        self.method = method
        self.error = error
        super().__init__(f"{method}: {error.get('code')}: {error.get('message')}")


@dataclass
class RecordedFrame:
    """One inbound frame, in the order this client actually received it.

    `kind` is `"response"` for a reply matched to one of this client's
    own `call()`s by `id`, or `"update"` for anything else (an
    unsolicited `session/update` notification, or an agent-initiated
    request such as `session/request_permission` -- this client never
    answers those, matching the fact that recovery tests care about
    ordering/delivery, not permission-flow behavior).
    """

    kind: str
    value: dict[str, Any]


@dataclass
class AcpWsClient:
    """A single `/ws` connection: sends JSON-RPC requests, awaits their
    matching replies by `id`, and records every frame -- replies and
    live push notifications alike -- in the exact order they arrived on
    the wire, so a test can assert real delivery ordering (e.g. "the
    replayed backlog arrived before the live tail") rather than just
    "the call eventually returned something".
    """

    base_url: str
    token: str | None = None
    connect_timeout: float = 5.0
    _ids: itertools.count = field(default_factory=lambda: itertools.count(1))
    _ws: Any = field(default=None, init=False, repr=False)
    _loop: asyncio.AbstractEventLoop = field(
        default_factory=asyncio.new_event_loop, init=False, repr=False
    )
    frames: list[RecordedFrame] = field(default_factory=list, init=False)

    def connect(self) -> None:
        """Opens the WebSocket connection. Idempotent-ish: reconnecting
        (calling this again after `close()`) is exactly how a test
        exercises `_acpx.resume` -- a fresh TCP/WS connection, not the
        same one, is the realistic reconnect shape a real client sees
        after a network blip or a client-side restart."""
        url = self.base_url.replace("http://", "ws://").replace("https://", "wss://")
        headers = {} if self.token is None else {"Authorization": f"Bearer {self.token}"}
        self._ws = self._run(
            websockets.connect(
                f"{url}/ws",
                additional_headers=headers,
                open_timeout=self.connect_timeout,
            )
        )

    def close(self) -> None:
        if self._ws is not None:
            self._run(self._ws.close())
            self._ws = None

    def call(
        self,
        method: str,
        params: dict[str, Any],
        *,
        resume: tuple[int, str] | None = None,
        timeout: float = 5.0,
    ) -> dict[str, Any]:
        """Sends a request and blocks until its matching reply arrives,
        recording every frame seen in between (and the reply itself) in
        `self.frames`. `resume=(last_seq, epoch)` injects the real
        `_acpx.resume` reconnect cursor -- see `live.take_resume_cursor`
        -- into `params` exactly like a real reconnecting client would.
        """
        request_id = next(self._ids)
        if resume is not None:
            last_seq, epoch = resume
            params = dict(params)
            params["_acpx"] = {"resume": {"lastSeq": last_seq, "epoch": epoch}}
        payload = {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
        return self._run(self._call_async(payload, request_id, timeout))

    def notify(self, method: str, params: dict[str, Any]) -> None:
        """Sends a true JSON-RPC notification (no `id`, no reply
        expected) -- e.g. `session/cancel`, which per the real ACP
        `CancelNotification` schema never gets a response frame."""
        payload = {"jsonrpc": "2.0", "method": method, "params": params}
        self._run(self._ws.send(json.dumps(payload)))

    def drain(self, *, count: int, timeout: float = 5.0) -> list[RecordedFrame]:
        """Reads and records up to `count` more frames without sending
        anything -- used to observe live `session/update` push that
        arrives on its own, independent of any `call()`."""
        return self._run(self._drain_async(count, timeout))

    async def _call_async(
        self, payload: dict[str, Any], request_id: int, timeout: float
    ) -> dict[str, Any]:
        await self._ws.send(json.dumps(payload))
        async with asyncio.timeout(timeout):
            while True:
                raw = await self._ws.recv()
                frame = json.loads(raw)
                if frame.get("id") == request_id:
                    self.frames.append(RecordedFrame("response", frame))
                    if "error" in frame:
                        raise JsonRpcError(payload["method"], frame["error"])
                    return frame.get("result", {})
                self.frames.append(RecordedFrame("update", frame))

    async def _drain_async(self, count: int, timeout: float) -> list[RecordedFrame]:
        collected: list[RecordedFrame] = []
        async with asyncio.timeout(timeout):
            while len(collected) < count:
                raw = await self._ws.recv()
                frame = json.loads(raw)
                recorded = RecordedFrame("update", frame)
                collected.append(recorded)
                self.frames.append(recorded)
        return collected

    def _run(self, coro: Any) -> Any:
        return self._loop.run_until_complete(coro)
