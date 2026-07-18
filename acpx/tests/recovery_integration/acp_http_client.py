"""Small stdlib JSON-RPC client for ACPX HTTP recovery tests."""

from __future__ import annotations

import itertools
import json
from dataclasses import dataclass, field
from typing import Any
from urllib.error import HTTPError
from urllib.request import Request, urlopen


class JsonRpcError(RuntimeError):
    """A JSON-RPC error response returned by the ACPX daemon."""

    def __init__(self, method: str, error: dict[str, Any]) -> None:
        self.method = method
        self.error = error
        super().__init__(f"{method}: {error.get('code')}: {error.get('message')}")


@dataclass
class AcpHttpClient:
    """Dependency-free client for `/rpc` and `/health`."""

    base_url: str
    token: str | None = None
    _ids: itertools.count = field(default_factory=lambda: itertools.count(1))

    def call(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        request_id = next(self._ids)
        response = self._request(
            "/rpc",
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": params,
            },
        )
        if "error" in response:
            raise JsonRpcError(method, response["error"])
        return response.get("result", {})

    def health(self) -> dict[str, Any]:
        request = Request(f"{self.base_url}/health", method="GET", headers=self._headers())
        try:
            with urlopen(request, timeout=2) as response:
                return json.loads(response.read())
        except HTTPError as error:
            raise RuntimeError(f"health request failed: HTTP {error.code}") from error

    def _request(self, path: str, body: dict[str, Any]) -> dict[str, Any]:
        encoded = json.dumps(body).encode("utf-8")
        headers = self._headers()
        headers["Content-Type"] = "application/json"
        request = Request(
            f"{self.base_url}{path}",
            data=encoded,
            method="POST",
            headers=headers,
        )
        try:
            with urlopen(request, timeout=5) as response:
                return json.loads(response.read())
        except HTTPError as error:
            raise RuntimeError(f"{path} request failed: HTTP {error.code}") from error

    def _headers(self) -> dict[str, str]:
        return {} if self.token is None else {"Authorization": f"Bearer {self.token}"}
