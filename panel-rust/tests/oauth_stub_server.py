#!/usr/bin/env python3
"""Real (stand-in) MCP OAuth 2.1 authorization server for
`live_oauth_browser_e2e.sh`. Not a mock of `acpx_core::oauth` -- a real
HTTP server, implementing exactly the endpoints that module's discovery/
DCR/PKCE/exchange code calls, so a real browser driving it exercises the
actual acpx code path over real HTTP, not a stub inside the test process.

Endpoints:
  GET  /.well-known/oauth-protected-resource    -> 404 (falls back to
                                                      treating this
                                                      server's own origin
                                                      as the auth server)
  GET  /.well-known/oauth-authorization-server  -> RFC 8414 metadata
  POST /register                                -> RFC 7591 DCR,
                                                      always issues
                                                      "browser-test-client"
  GET  /authorize                                -> a real HTML consent
                                                      page with an
                                                      "Approve" button
                                                      (id="approve-button")
                                                      a real browser must
                                                      click -- not an
                                                      instant redirect
  POST /approve                                  -> issues the redirect
                                                      back to redirect_uri
                                                      with a fixed test
                                                      code+the submitted
                                                      state
  POST /token                                    -> authorization_code
                                                      and refresh_token
                                                      grants; if
                                                      FAIL_TOKEN=1 in the
                                                      environment, always
                                                      500s (for the
                                                      failure-path check)

Usage: oauth_stub_server.py <port>  (0 = OS-assigned ephemeral port).
Prints `STUB_OAUTH_ORIGIN=http://127.0.0.1:<port>` to stdout once bound.
"""
import json
import os
import sys
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 0
ORIGIN = None  # filled in once bound, below
FAIL_TOKEN = os.environ.get("FAIL_TOKEN") == "1"


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        sys.stderr.write("[oauth-stub-server] " + (fmt % args) + "\n")

    def _send(self, status, body, content_type="application/json"):
        data = body.encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        parsed = urllib.parse.urlparse(self.path)
        path = parsed.path
        query = urllib.parse.parse_qs(parsed.query)

        if path == "/.well-known/oauth-protected-resource":
            self._send(404, "")
            return
        if path == "/.well-known/oauth-authorization-server":
            self._send(200, json.dumps({
                "authorization_endpoint": f"{ORIGIN}/authorize",
                "token_endpoint": f"{ORIGIN}/token",
                "registration_endpoint": f"{ORIGIN}/register",
            }))
            return
        if path == "/authorize":
            redirect_uri = query.get("redirect_uri", [""])[0]
            state = query.get("state", [""])[0]
            client_id = query.get("client_id", [""])[0]
            html = f"""<!doctype html>
<html><head><title>Stub MCP Authorization Server</title></head>
<body style="font-family: sans-serif; max-width: 480px; margin: 80px auto;">
  <h2>Authorize acpx</h2>
  <p>Client <code>{client_id}</code> is requesting access to your MCP server.</p>
  <form method="POST" action="/approve">
    <input type="hidden" name="redirect_uri" value="{redirect_uri}">
    <input type="hidden" name="state" value="{state}">
    <button type="submit" id="approve-button" style="padding: 10px 20px; font-size: 16px;">Approve</button>
  </form>
</body></html>"""
            self._send(200, html, content_type="text/html")
            return
        self._send(404, "")

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length).decode("utf-8") if length else ""

        if self.path == "/register":
            self._send(200, json.dumps({"client_id": "browser-test-client"}))
            return

        if self.path == "/approve":
            form = urllib.parse.parse_qs(body)
            redirect_uri = form.get("redirect_uri", [""])[0]
            state = form.get("state", [""])[0]
            location = f"{redirect_uri}?code=browser-test-code&state={urllib.parse.quote(state)}"
            self.send_response(302)
            self.send_header("Location", location)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return

        if self.path == "/token":
            if FAIL_TOKEN:
                self._send(500, json.dumps({"error": "server_error", "error_description": "stub forced failure"}))
                return
            form = urllib.parse.parse_qs(body)
            grant_type = form.get("grant_type", [""])[0]
            if grant_type == "authorization_code":
                self._send(200, json.dumps({
                    "access_token": "browser-test-access-token",
                    "refresh_token": "browser-test-refresh-token",
                    "expires_in": 3600,
                }))
                return
            if grant_type == "refresh_token":
                self._send(200, json.dumps({
                    "access_token": "browser-test-refreshed-access-token",
                    "refresh_token": "browser-test-refresh-token",
                    "expires_in": 3600,
                }))
                return
            self._send(400, json.dumps({"error": "unsupported_grant_type"}))
            return

        self._send(404, "")


if __name__ == "__main__":
    server = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    actual_port = server.server_address[1]
    ORIGIN = f"http://127.0.0.1:{actual_port}"
    print(f"STUB_OAUTH_ORIGIN={ORIGIN}", flush=True)
    server.serve_forever()
