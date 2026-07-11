#!/usr/bin/env bash
# Headless live Qt/C-ABI SAP exercise for the two newly-wired real FFI
# methods: edit.appendClip and playback.getFrame. Sibling to
# run-headless-ffi-sap.sh (which only exercises addTrack/listTracks) --
# this script does NOT modify that one.
#
# Flow against a real headless Shotcut binary (built with corrosion +
# real_ffi, per shotcut/CMakeLists.txt / shotcut/src/rustbridge/sap_ffi.*):
#   sap.hello -> project.select -> edit.addTrack -> edit.appendClip (real
#   test video asset) -> playback.getFrame -> decode + assert the returned
#   bytes are a valid, non-blank JPEG (not all-black/uniform).
#
# Startup env (shotcut/src/main.cpp):
#   SNAPSHOT_HEADLESS=1  -> QT_QPA_PLATFORM=offscreen before QApplication
#   SNAPSHOT_SAP_SOCKET  -> path for Unix socket; starts sap_start_server
#   SNAPSHOT_SAP_TOKEN   -> token required by sap.hello
#
# Wire protocol (sap-rust framing.rs + server.rs): LSP-style Content-Length
# headers, then raw JSON-RPC 2.0 body. Same framing as run-headless-ffi-sap.sh.
#
# Usage:
#   scripts/run-headless-ffi-sap-appendclip-getframe.sh
# Optional env:
#   SHOTCUT_BIN=/path/to/shotcut     # skip auto-find
#   TEST_ASSET=/path/to/video.mp4    # skip auto-generate via ffmpeg
#   SAP_TOKEN=test-token
#   SAP_SOCKET=/tmp/....sock
#   SOCKET_WAIT_SECS=60
#   SHOTCUT_WAIT_SECS=90

set -euo pipefail

SCRATCH="${SCRATCH:-/tmp/grok-goal-fdf69df3d51a/implementer}"
BUILD="${BUILD:-$SCRATCH/shotcut-build}"
SAP_TOKEN="${SAP_TOKEN:-test-token}"
SAP_SOCKET="${SAP_SOCKET:-$SCRATCH/headless-ffi-sap-appendclip.sock}"
SOCKET_WAIT_SECS="${SOCKET_WAIT_SECS:-60}"
SHOTCUT_WAIT_SECS="${SHOTCUT_WAIT_SECS:-90}"
PROJECT_ID="${PROJECT_ID:-headless-ffi-appendclip-proj}"
LOG_DIR="${LOG_DIR:-$SCRATCH}"
SHOTCUT_LOG="${SHOTCUT_LOG:-$LOG_DIR/headless-shotcut-appendclip.log}"
CLIENT_LOG="${CLIENT_LOG:-$LOG_DIR/headless-sap-client-appendclip.log}"
ASSET_DIR="${ASSET_DIR:-$SCRATCH/assets}"
TEST_ASSET="${TEST_ASSET:-$ASSET_DIR/test-clip.mp4}"

SHOTCUT_PID=""
cleanup() {
  local ec=$?
  if [[ -n "${SHOTCUT_PID}" ]] && kill -0 "${SHOTCUT_PID}" 2>/dev/null; then
    echo "==> cleanup: killing shotcut pid=${SHOTCUT_PID}"
    kill "${SHOTCUT_PID}" 2>/dev/null || true
    for _ in 1 2 3 4 5; do
      kill -0 "${SHOTCUT_PID}" 2>/dev/null || break
      sleep 0.2
    done
    kill -9 "${SHOTCUT_PID}" 2>/dev/null || true
    wait "${SHOTCUT_PID}" 2>/dev/null || true
  fi
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

echo "=== headless live Qt/C-ABI SAP exercise: appendClip + getFrame ==="
echo "scratch:  ${SCRATCH}"
echo "build:    ${BUILD}"
echo "socket:   ${SAP_SOCKET}"
echo "token:    ${SAP_TOKEN}"
echo "project:  ${PROJECT_ID}"
echo "asset:    ${TEST_ASSET}"

SHOTCUT_BIN_RESOLVED=""
if ! SHOTCUT_BIN_RESOLVED="$(find_shotcut)"; then
  echo "BINARY_MISSING: no executable 'shotcut' under ${BUILD}"
  echo "  Build first:"
  echo "    cmake -S shotcut -B ${BUILD} -G Ninja"
  echo "    cmake --build ${BUILD} -j\"\$(nproc)\""
  exit 2
fi
echo "binary:   ${SHOTCUT_BIN_RESOLVED}"

mkdir -p "${ASSET_DIR}"
if [[ ! -s "${TEST_ASSET}" ]]; then
  echo "==> generating test asset via ffmpeg (colorful test pattern, not black)"
  if ! command -v ffmpeg >/dev/null 2>&1; then
    echo "ERROR: ffmpeg not found and TEST_ASSET (${TEST_ASSET}) does not exist"
    exit 2
  fi
  ffmpeg -y -loglevel error -f lavfi -i "testsrc2=size=320x240:rate=25:duration=3" \
    -pix_fmt yuv420p "${TEST_ASSET}"
fi
if [[ ! -s "${TEST_ASSET}" ]]; then
  echo "ERROR: test asset ${TEST_ASSET} missing/empty after generation attempt"
  exit 2
fi

rm -f "${SAP_SOCKET}"
mkdir -p "$(dirname "${SAP_SOCKET}")" "${LOG_DIR}"

echo "==> starting shotcut headless with SAP socket"
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

sleep 0.3

echo "==> JSON-RPC over Content-Length framing"
python3 - "${SAP_SOCKET}" "${SAP_TOKEN}" "${PROJECT_ID}" "${CLIENT_LOG}" "${TEST_ASSET}" <<'PY'
#!/usr/bin/env python3
import base64
import io
import json
import socket
import sys
import time

sock_path, token, project_id, client_log, asset_path = sys.argv[1:6]


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
        req = {"jsonrpc": "2.0", "id": rid, "method": method, "params": params}
        self.write_message(req)
        while True:
            msg = self.read_message()
            if "id" not in msg or msg.get("id") is None:
                log(f"  [notification] {json.dumps(msg, ensure_ascii=False)}")
                continue
            if msg.get("id") != rid:
                log(f"  [unexpected id] want={rid} got={msg.get('id')}: {json.dumps(msg, ensure_ascii=False)}")
                continue
            return msg


def require_ok(resp: dict, label: str):
    if resp.get("error"):
        log(f"FAIL {label}: error={json.dumps(resp['error'], ensure_ascii=False)}")
        raise SystemExit(4)
    if "result" not in resp:
        log(f"FAIL {label}: no result field: {json.dumps(resp, ensure_ascii=False)}")
        raise SystemExit(4)
    result = resp["result"]
    log(f"OK   {label}: result={json.dumps(result, ensure_ascii=False)[:400]}")
    return result


open(client_log, "w", encoding="utf-8").close()

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

    require_ok(client.call("sap.hello", {"token": token}), "sap.hello")
    require_ok(client.call("project.select", {"projectId": project_id}), "project.select")

    track = require_ok(client.call("edit.addTrack", {"kind": "video"}), "edit.addTrack")
    if not isinstance(track, dict) or "index" not in track:
        log(f"FAIL edit.addTrack: expected object with index, got {track}")
        raise SystemExit(4)
    track_index = track["index"]

    # --- edit.appendClip: real source path, not clipboard/"current source" ---
    clip = require_ok(
        client.call("edit.appendClip", {"trackIndex": track_index, "source": {"path": asset_path}}),
        "edit.appendClip",
    )
    if not isinstance(clip, dict) or "index" not in clip or "clipId" not in clip:
        log(f"FAIL edit.appendClip: expected object with index/clipId, got {clip}")
        raise SystemExit(4)
    out_frame = clip.get("outFrame", 0)
    if not isinstance(out_frame, (int, float)) or out_frame <= 0:
        log(f"FAIL edit.appendClip: expected positive outFrame reflecting the real appended clip, got {clip}")
        raise SystemExit(4)
    log(f"appended clip: {json.dumps(clip, ensure_ascii=False)}")

    # edit.listClips is a separate, pre-existing stub in ffi_backend.rs
    # (`Ok(Vec::new())`) that was never in scope for this pass (only
    # edit.appendClip and playback.getFrame were). Call it for visibility
    # but do not hard-fail the exercise on it -- appendClip's own result
    # (clipId/index/inFrame/outFrame from the real MultitrackModel) is the
    # actual proof the append happened.
    listed = require_ok(client.call("edit.listClips", {"trackIndex": track_index}), "edit.listClips")
    if not isinstance(listed, list) or len(listed) < 1:
        log(f"NOTE edit.listClips: still a stub (returned {listed}); out of scope for "
            f"appendClip/getFrame wiring -- not treated as a failure here")

    # --- playback.getFrame: pick a frame inside the appended clip's range ---
    frame_number = min(10, int(out_frame))
    frame_resp = require_ok(
        client.call("playback.getFrame", {"frame": frame_number, "format": "jpeg"}),
        "playback.getFrame",
    )
    if not isinstance(frame_resp, dict) or "data" not in frame_resp:
        log(f"FAIL playback.getFrame: expected object with data, got {frame_resp}")
        raise SystemExit(4)

    raw = base64.b64decode(frame_resp["data"])
    if len(raw) < 100:
        log(f"FAIL playback.getFrame: decoded byte length suspiciously small ({len(raw)} bytes)")
        raise SystemExit(4)
    if raw[0:2] != b"\xff\xd8":
        log(f"FAIL playback.getFrame: decoded bytes do not start with the JPEG SOI marker (0xFFD8); got {raw[0:8].hex()}")
        raise SystemExit(4)

    try:
        from PIL import Image
    except ImportError:
        log("FAIL playback.getFrame: Pillow (PIL) not available to decode/validate the JPEG")
        raise SystemExit(4)

    img = Image.open(io.BytesIO(raw))
    img.load()  # force full decode -- raises if the JPEG is corrupt/truncated
    if img.format != "JPEG":
        log(f"FAIL playback.getFrame: Pillow reports format={img.format}, expected JPEG")
        raise SystemExit(4)
    width, height = img.size
    if width <= 0 or height <= 0:
        log(f"FAIL playback.getFrame: decoded image has non-positive dimensions {img.size}")
        raise SystemExit(4)

    rgb = img.convert("RGB")
    extrema = rgb.getextrema()  # ((rmin,rmax),(gmin,gmax),(bmin,bmax))
    all_zero = all(mx == 0 for _lo, mx in extrema)
    no_variance = all(lo == hi for lo, hi in extrema)
    if all_zero:
        log(f"FAIL playback.getFrame: decoded JPEG is all-black (extrema={extrema})")
        raise SystemExit(4)
    if no_variance:
        log(f"FAIL playback.getFrame: decoded JPEG is a uniform/blank color (extrema={extrema})")
        raise SystemExit(4)

    log(f"SUCCESS: appendClip + getFrame against live headless Shotcut")
    log(f"  projectId={project_id}")
    log(f"  track={json.dumps(track, ensure_ascii=False)}")
    log(f"  appended_clip={json.dumps(clip, ensure_ascii=False)}")
    log(f"  requested_frame={frame_number}")
    log(f"  jpeg_bytes={len(raw)} dims={width}x{height} rgb_extrema={extrema}")
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
