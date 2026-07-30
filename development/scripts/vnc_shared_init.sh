#!/usr/bin/env bash
# Host-side shared Xvnc lifecycle and workspace registry.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
REG="${PORT_REGISTRY_SCRIPT:-$REPO_ROOT/memory/team/reserved/port_registry.sh}"
STATE_BASE="${VNC_STATE_BASE:-${HOME}/.snapflow/state}"
ROOT="${VNC_SHARED_ROOT:-$STATE_BASE/snapflow/vnc-shared}"
PORT_REGISTRY_FILE="${PORT_REGISTRY_FILE:-$ROOT/ports.json}"
export PORT_REGISTRY_FILE
REGISTRY="$ROOT/workspaces.tsv"
LOCK="$ROOT/workspaces.lock"
SERVER_LABEL="${VNC_SHARED_LABEL:-shared-xvnc}"

die() { echo "error: $*" >&2; exit 1; }

worktree_path() {
  local input="${1:-.}"
  git -C "$input" rev-parse --show-toplevel 2>/dev/null || realpath "$input"
}

worktree_id() {
  local path label hash
  path="$(worktree_path "$1")"
  label="$(git -C "$path" rev-parse --abbrev-ref HEAD 2>/dev/null || basename "$path")"
  label="$(printf '%s' "$label" | sed 's#[^A-Za-z0-9_.-]#-#g')"
  hash="$(printf '%s' "$path" | sha256sum | cut -c1-8)"
  printf '%s-%s\n' "$label" "$hash"
}

ensure_files() {
  mkdir -p "$ROOT"
  touch "$REGISTRY"
}

state_get() {
  [ -f "$ROOT/server.env" ] || return 1
  # shellcheck source=/dev/null
  source "$ROOT/server.env"
  [ -n "${xvnc_pid:-}" ] && kill -0 "$xvnc_pid" 2>/dev/null
}

cmd_init() {
  ensure_files
  if state_get; then
    cat "$ROOT/server.env"
    return 0
  fi
  command -v Xvnc >/dev/null || die "Xvnc is required on the host"
  command -v fluxbox >/dev/null || die "fluxbox is required on the host"

  (
    flock -w 10 9 || die "could not lock shared VNC state"
    if state_get; then cat "$ROOT/server.env"; exit 0; fi
    if [ -f "$ROOT/server.env" ]; then
      # A previous host session may have died after writing state. Remove its
      # registry reservation before creating the replacement server.
      # shellcheck source=/dev/null
      source "$ROOT/server.env"
      [ -z "${vnc_port:-}" ] || "$REG" release "$vnc_port" || true
      rm -f "$ROOT/server.env"
    fi
    local display=100
    while [ -e "/tmp/.X11-unix/X$display" ]; do display=$((display + 1)); done
    local vnc_port
    vnc_port="$("$REG" reserve "$SERVER_LABEL" "shared host Xvnc")"
    local fluxbox_home="$ROOT/fluxbox"
    mkdir -p "$fluxbox_home"
    cat > "$fluxbox_home/init" <<EOF
session.screen0.workspaces: 32
session.screen0.toolbar.visible: true
session.screen0.toolbar.autoHide: false
session.screen0.toolbar.placement: BottomCenter
session.screen0.toolbar.widthPercent: 100
session.screen0.toolbar.layer: Above
session.screen0.toolbar.maxOver: true
session.screen0.toolbar.tools: prevworkspace, workspacename, nextworkspace, clock, iconbar
session.screen0.allowRemoteActions: true
EOF
    # Qt/X11 clients can issue MIT-SHM requests that are unsafe across the
    # host/container boundary used by the VNC launcher.  Keep the X server
    # in the plain-pixmap path; vnc_worktree.sh also sets the matching Qt
    # client-side guard below.
    # Bind on all interfaces so the Makefile-managed VNC endpoint can be
    # reached from the operator's VNC client. Security is intentionally
    # delegated to the surrounding network/SSH boundary; this development
    # server has no VNC password.
    setsid nohup Xvnc ":$display" -geometry 2560x1440 -depth 24 -noreset \
      -extension MIT-SHM \
      -SecurityTypes None -rfbport "$vnc_port" \
      > "$ROOT/xvnc.log" 2>&1 9>&- &
    local xvnc_pid=$!
    for _ in $(seq 1 50); do
      [ -S "/tmp/.X11-unix/X$display" ] && break
      sleep 0.1
    done
    [ -S "/tmp/.X11-unix/X$display" ] || {
      kill "$xvnc_pid" 2>/dev/null || true
      "$REG" release "$vnc_port" || true
      die "Xvnc did not create display :$display; see $ROOT/xvnc.log"
    }
    DISPLAY=":$display" setsid nohup fluxbox -rc "$fluxbox_home/init" \
      > "$ROOT/fluxbox.log" 2>&1 9>&- &
    local wm_pid=$!
    {
      printf 'vnc_port=%s\n' "$vnc_port"
      printf 'display=:%s\n' "$display"
      printf 'xvnc_pid=%s\n' "$xvnc_pid"
      printf 'wm_pid=%s\n' "$wm_pid"
      printf 'root=%s\n' "$ROOT"
    } > "$ROOT/server.env"
    cat "$ROOT/server.env"
  ) 9>"$LOCK"
}

cmd_workspace() {
  local action="${1:?workspace action required}" worktree="${2:-.}"
  ensure_files
  local path id
  path="$(worktree_path "$worktree")"
  id="$(worktree_id "$path")"
  case "$action" in
    ensure)
      (
        flock -w 10 9 || die "could not lock workspace registry"
        local existing
        existing="$(awk -F '\t' -v id="$id" '$1 == id { print $3; exit }' "$REGISTRY")"
        if [ -n "$existing" ]; then echo "$existing"; exit 0; fi
        local workspace=0
        while awk -F '\t' -v ws="$workspace" '$3 == ws { found=1 } END { exit found ? 0 : 1 }' "$REGISTRY"; do
          workspace=$((workspace + 1))
        done
        printf '%s\t%s\t%s\t%s\n' "$id" "$path" "$workspace" "$(date -Iseconds)" >> "$REGISTRY"
        echo "$workspace"
      ) 9>"$LOCK"
      ;;
    release)
      (
        flock -w 10 9 || die "could not lock workspace registry"
        awk -F '\t' -v id="$id" '$1 != id' "$REGISTRY" > "$REGISTRY.tmp"
        mv "$REGISTRY.tmp" "$REGISTRY"
      ) 9>"$LOCK"
      ;;
    place)
      local pid="${3:?window pid required}" workspace="${4:?workspace number required}"
      if command -v wmctrl >/dev/null 2>&1; then
        local window_id=""
        # Qt can take several seconds to create the top-level window while
        # loading filters and initializing the embedded Rust panel.  The
        # launcher used to give up after 5 seconds, leaving a live process
        # registered to a workspace but visibly stuck on the current desktop.
        for _ in $(seq 1 600); do
          window_id="$(wmctrl -lp 2>/dev/null | awk -v pid="$pid" '$3 == pid { print $1; exit }')"
          [ -n "$window_id" ] && break
          sleep 0.1
        done
        [ -n "$window_id" ] || die "could not locate window for pid $pid"
        wmctrl -i -r "$window_id" -t "$workspace"
        return 0
      fi
      command -v python3 >/dev/null 2>&1 || die "python3 is required for X11 workspace placement"
      TARGET_PID="$pid" TARGET_DESKTOP="$workspace" python3 - <<'PY'
import os
import time
from Xlib import X, display
from Xlib.protocol import event

target_pid = int(os.environ["TARGET_PID"])
target_desktop = int(os.environ["TARGET_DESKTOP"])
d = display.Display()
root = d.screen().root
client_list_atom = d.intern_atom("_NET_CLIENT_LIST")
pid_atom = d.intern_atom("_NET_WM_PID")
desktop_atom = d.intern_atom("_NET_WM_DESKTOP")
target = None
stable = 0
for _ in range(600):
    if target is None:
        prop = root.get_full_property(client_list_atom, X.AnyPropertyType)
        window_ids = [] if prop is None else prop.value
        for window_id in window_ids:
            window = d.create_resource_object("window", int(window_id))
            try:
                pid_prop = window.get_full_property(pid_atom, X.AnyPropertyType)
                if pid_prop is not None and int(pid_prop.value[0]) == target_pid:
                    target = window
                    break
            except Exception:
                continue
    if target is not None:
        try:
            root.send_event(
                event.ClientMessage(
                    window=target,
                    client_type=desktop_atom,
                    data=(32, [target_desktop, X.CurrentTime, 0, 0, 0]),
                ),
                event_mask=X.SubstructureRedirectMask | X.SubstructureNotifyMask,
            )
            d.sync()
            desktop = target.get_full_property(desktop_atom, X.AnyPropertyType)
            if desktop is not None and int(desktop.value[0]) == target_desktop:
                stable += 1
                if stable >= 10:
                    print(f"placed pid {target_pid} on workspace {target_desktop}")
                    break
            else:
                stable = 0
        except Exception:
            target = None
            stable = 0
    time.sleep(0.1)
else:
    raise SystemExit(f"window for pid {target_pid} did not settle on workspace {target_desktop}")
PY
      ;;
    *) die "unknown workspace action: $action" ;;
  esac
}

cmd_status() {
  ensure_files
  if state_get; then echo "shared-vnc: running"; cat "$ROOT/server.env"; else echo "shared-vnc: stopped"; fi
  echo "workspaces:"
  cat "$REGISTRY"
}

cmd_down() {
  ensure_files
  (
    flock -w 10 9 || die "could not lock shared VNC state"
    [ ! -s "$REGISTRY" ] || die "cannot stop shared VNC while workspaces are active"
    if [ -f "$ROOT/server.env" ]; then
      # shellcheck source=/dev/null
      source "$ROOT/server.env"
      kill "${wm_pid:-}" "${xvnc_pid:-}" 2>/dev/null || true
      "$REG" release "${vnc_port:-}" 2>/dev/null || true
      rm -f "$ROOT/server.env"
    fi
  ) 9>"$LOCK"
}

case "${1:-}" in
  init) shift; cmd_init "$@" ;;
  workspace-ensure) shift; cmd_workspace ensure "$@" ;;
  workspace-release) shift; cmd_workspace release "$@" ;;
  workspace-place) shift; cmd_workspace place "$@" ;;
  status) cmd_status ;;
  down) cmd_down ;;
  id) shift; worktree_id "${1:-.}" ;;
  *) die "usage: $0 {init|workspace-ensure <worktree>|workspace-release <worktree>|status|down|id <worktree>}" ;;
esac
