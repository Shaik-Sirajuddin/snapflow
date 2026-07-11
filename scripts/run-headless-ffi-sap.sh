#!/usr/bin/env bash
# Headless live Qt/C-ABI SAP exercise against a Shotcut binary built with
# corrosion + real_ffi (FfiBackend inside the process).
#
# Startup env (shotcut/src/main.cpp):
#   SNAPSHOT_HEADLESS=1  → QT_QPA_PLATFORM=offscreen before QApplication
#   SNAPSHOT_SAP_SOCKET  → path for Unix socket; starts sap_start_server
#   SNAPSHOT_SAP_TOKEN   → token required by sap.hello
#
# Wire protocol (sap-rust framing.rs + server.rs):
#   LSP-style Content-Length headers, then raw JSON-RPC 2.0 body.
#   Handshake: sap.hello {token} → project.select {projectId} → edit.*
#
# Usage:
#   /tmp/grok-goal-fdf69df3d51a/implementer/run-headless-ffi-sap.sh
# Optional env:
#   SHOTCUT_BIN=/path/to/shotcut   # skip auto-find
#   SAP_TOKEN=test-token
#   SAP_SOCKET=/tmp/....sock
#   SOCKET_WAIT_SECS=60
#   SHOTCUT_WAIT_SECS=90

set -euo pipefail

SCRATCH="${SCRATCH:-/tmp/grok-goal-fdf69df3d51a/implementer}"
BUILD="${BUILD:-$SCRATCH/shotcut-build}"
SAP_TOKEN="${SAP_TOKEN:-test-token}"
SAP_SOCKET="${SAP_SOCKET:-$SCRATCH/headless-ffi-sap.sock}"
SOCKET_WAIT_SECS="${SOCKET_WAIT_SECS:-60}"
SHOTCUT_WAIT_SECS="${SHOTCUT_WAIT_SECS:-90}"
PROJECT_ID="${PROJECT_ID:-headless-ffi-proj}"
LOG_DIR="${LOG_DIR:-$SCRATCH}"
SHOTCUT_LOG="${SHOTCUT_LOG:-$LOG_DIR/headless-shotcut.log}"
CLIENT_LOG="${CLIENT_LOG:-$LOG_DIR/headless-sap-client.log}"

SHOTCUT_PID=""
cleanup() {
  local ec=$?
  if [[ -n "${SHOTCUT_PID}" ]] && kill -0 "${SHOTCUT_PID}" 2>/dev/null; then
    echo "==> cleanup: killing shotcut pid=${SHOTCUT_PID}"
    kill "${SHOTCUT_PID}" 2>/dev/null || true
    # Give it a moment, then force.
    for _ in 1 2 3 4 5; do
      kill -0 "${SHOTCUT_PID}" 2>/dev/null || break
      sleep 0.2
    done
    kill -9 "${SHOTCUT_PID}" 2>/dev/null || true
    wait "${SHOTCUT_PID}" 2>/dev/null || true
  fi
  # Socket file is owned by the server; remove leftover path if present.
  rm -f "${SAP_SOCKET}" 2>/dev/null || true
  return "${ec}"
}
trap cleanup EXIT INT TERM

find_shotcut() {
  if [[ -n "${SHOTCUT_BIN:-}" ]]; then
    if [[ -x "${SHOTCUT_BIN}" ]]; then
      echo "${SHOTCUT_BIN}"
      return 0
    fi
    echo "ERROR: SHOTCUT_BIN=${SHOTCUT_BIN} is not executable" >&2
    return 1
  fi

  local candidates=(
    "${BUILD}/src/shotcut"
    "${BUILD}/shotcut"
  )
  local c
  for c in "${candidates[@]}"; do
    if [[ -x "${c}" ]]; then
      echo "${c}"
      return 0
    fi
  done

  # Broader search under the build tree (cmake generator-dependent layout).
  if [[ -d "${BUILD}" ]]; then
    local found
    found="$(find "${BUILD}" -name shotcut -type f -perm -111 2>/dev/null | head -n 1 || true)"
    if [[ -n "${found}" ]]; then
      echo "${found}"
      return 0
    fi
  fi
  return 1
}

echo "=== headless live Qt/C-ABI SAP exercise ==="
echo "scratch:  ${SCRATCH}"
echo "build:    ${BUILD}"
echo "socket:   ${SAP_SOCKET}"
echo "token:    ${SAP_TOKEN}"
echo "project:  ${PROJECT_ID}"

SHOTCUT_BIN_RESOLVED=""
if ! SHOTCUT_BIN_RESOLVED="$(find_shotcut)"; then
  echo "BINARY_MISSING: no executable 'shotcut' under ${BUILD}"
  echo "  Build is still in progress, or cmake output path differs."
  echo "  Re-run this script after:"
  echo "    cmake --build ${BUILD} -j\"\$(nproc)\""
  echo "  Typical binary path:"
  echo "    ${BUILD}/src/shotcut"
  exit 2
fi
echo "binary:   ${SHOTCUT_BIN_RESOLVED}"

rm -f "${SAP_SOCKET}"
mkdir -p "$(dirname "${SAP_SOCKET}")" "${LOG_DIR}"

echo "==> starting shotcut headless with SAP socket"
# SNAPSHOT_HEADLESS must be set before process start (main.cpp sets
# QT_QPA_PLATFORM=offscreen before QApplication construction).
SNAPSHOT_HEADLESS=1 \
SNAPSHOT_SAP_SOCKET="${SAP_SOCKET}" \
SNAPSHOT_SAP_TOKEN="${SAP_TOKEN}" \
  "${SHOTCUT_BIN_RESOLVED}" \
  >"${SHOTCUT_LOG}" 2>&1 &
SHOTCUT_PID=$!
echo "    pid=${SHOTCUT_PID}  log=${SHOTCUT_LOG}"

echo "==> waiting up to ${SOCKET_WAIT_SECS}s for socket ${SAP_SOCKET}"
deadline=$((SECONDS + SOCKET_WAIT_SECS))
while (( SECONDS < deadline )); do
  if [[ -S "${SAP_SOCKET}" ]]; then
    echo "    socket ready"
    break
  fi
  if ! kill -0 "${SHOTCUT_PID}" 2>/dev/null; then
    echo "ERROR: shotcut exited before creating socket (pid=${SHOTCUT_PID})"
    echo "---- last 80 lines of ${SHOTCUT_LOG} ----"
    tail -n 80 "${SHOTCUT_LOG}" 2>/dev/null || true
    exit 3
  fi
  sleep 0.25
done
if [[ ! -S "${SAP_SOCKET}" ]]; then
  echo "ERROR: socket not created within ${SOCKET_WAIT_SECS}s"
  echo "---- last 80 lines of ${SHOTCUT_LOG} ----"
  tail -n 80 "${SHOTCUT_LOG}" 2>/dev/null || true
  exit 3
fi

# Small settle: server accepts only after sap_start_server binds + listen.
sleep 0.3

echo "==> JSON-RPC over Content-Length framing"
# Embedded Python client matching sap-rust/src/framing.rs:
#   write: "Content-Length: N\r\n\r\n" + body
#   read:  headers until blank line, then exactly N body bytes
# Notifications (no "id") are skipped when waiting for a response.
python3 - "${SAP_SOCKET}" "${SAP_TOKEN}" "${PROJECT_ID}" "${CLIENT_LOG}" <<'PY'
#!/usr/bin/env python3
import json
import os
import socket
import sys
import time

sock_path, token, project_id, client_log = sys.argv[1:5]


def log(msg: str) -> None:
    line = f"{msg}\n"
    sys.stdout.write(line)
    sys.stdout.flush()
    with open(client_log, "a", encoding="utf-8") as f:
        f.write(line)


class SapClient:
    def __init__(self, path: str):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(30.0)
        self.sock.connect(path)
        self.rfile = self.sock.makefile("rb")
        self.next_id = 1

    def close(self) -> None:
        try:
            self.rfile.close()
        except Exception:
            pass
        try:
            self.sock.close()
        except Exception:
            pass

    def write_message(self, value: dict) -> None:
        body = json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
        header = f"Content-Length: {len(body)}\r\n\r\n".encode("ascii")
        self.sock.sendall(header + body)

    def read_message(self) -> dict:
        content_length = None
        while True:
            line = self.rfile.readline()
            if not line:
                raise EOFError("peer closed connection while reading headers")
            # Headers are ASCII; tolerate CRLF/LF.
            trimmed = line.decode("ascii", errors="replace").rstrip("\r\n")
            if trimmed == "":
                break
            if trimmed.lower().startswith("content-length:"):
                rest = trimmed.split(":", 1)[1].strip()
                content_length = int(rest)
        if content_length is None:
            raise ValueError("malformed Content-Length header (missing)")
        buf = self.rfile.read(content_length)
        if buf is None or len(buf) < content_length:
            raise EOFError("peer closed connection while reading body")
        return json.loads(buf.decode("utf-8"))

    def call(self, method: str, params: dict) -> dict:
        rid = self.next_id
        self.next_id += 1
        req = {
            "jsonrpc": "2.0",
            "id": rid,
            "method": method,
            "params": params,
        }
        self.write_message(req)
        # Skip unsolicited notifications (no id) until our response arrives.
        while True:
            msg = self.read_message()
            if "id" not in msg or msg.get("id") is None:
                log(f"  [notification] {json.dumps(msg, ensure_ascii=False)}")
                continue
            if msg.get("id") != rid:
                log(f"  [unexpected id] want={rid} got={msg.get('id')}: {json.dumps(msg, ensure_ascii=False)}")
                continue
            return msg


def require_ok(resp: dict, label: str) -> dict:
    if resp.get("error"):
        log(f"FAIL {label}: error={json.dumps(resp['error'], ensure_ascii=False)}")
        raise SystemExit(4)
    if "result" not in resp:
        log(f"FAIL {label}: no result field: {json.dumps(resp, ensure_ascii=False)}")
        raise SystemExit(4)
    log(f"OK   {label}: result={json.dumps(resp['result'], ensure_ascii=False)}")
    return resp["result"]


# Clear prior client log for this run.
open(client_log, "w", encoding="utf-8").close()

# Connect with a short retry (race: socket exists but accept not yet ready).
client = None
last_err = None
for attempt in range(40):
    try:
        client = SapClient(sock_path)
        break
    except (ConnectionRefusedError, FileNotFoundError, OSError) as e:
        last_err = e
        time.sleep(0.25)
if client is None:
    log(f"FAIL connect to {sock_path}: {last_err}")
    raise SystemExit(3)

try:
    log(f"connected to {sock_path}")

    # Gate 1: token handshake (server.rs: sap.hello params.token)
    hello = client.call("sap.hello", {"token": token})
    require_ok(hello, "sap.hello")

    # Gate 2: bind project (params use camelCase projectId)
    select = client.call("project.select", {"projectId": project_id})
    select_result = require_ok(select, "project.select")

    # Mutating call via FfiBackend → TimelineDock::addVideoTrack
    added = client.call("edit.addTrack", {"kind": "video"})
    track = require_ok(added, "edit.addTrack")
    if not isinstance(track, dict):
        log(f"FAIL edit.addTrack: expected object track, got {type(track).__name__}")
        raise SystemExit(4)

    # Read-back via MultitrackModel::trackList
    listed = client.call("edit.listTracks", {})
    tracks = require_ok(listed, "edit.listTracks")
    if not isinstance(tracks, list):
        log(f"FAIL edit.listTracks: expected list, got {type(tracks).__name__}")
        raise SystemExit(4)
    if len(tracks) < 1:
        log("FAIL edit.listTracks: empty track list after addTrack")
        raise SystemExit(4)

    log(f"SUCCESS: addTrack + listTracks against live headless Shotcut")
    log(f"  projectId={project_id}")
    log(f"  added_track={json.dumps(track, ensure_ascii=False)}")
    log(f"  tracks_count={len(tracks)}")
    log(f"  tracks={json.dumps(tracks, ensure_ascii=False)}")
    # project.select result for debugging
    log(f"  select_state_keys={list(select_result.keys()) if isinstance(select_result, dict) else type(select_result).__name__}")
finally:
    client.close()

raise SystemExit(0)
PY
client_ec=$?

if [[ "${client_ec}" -eq 0 ]]; then
  echo "==> exercise PASSED (exit 0)"
  exit 0
else
  echo "==> exercise FAILED (client exit ${client_ec})"
  echo "---- last 80 lines of ${SHOTCUT_LOG} ----"
  tail -n 80 "${SHOTCUT_LOG}" 2>/dev/null || true
  exit "${client_ec}"
fi
