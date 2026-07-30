#!/usr/bin/env bash
# Slint hot-reload viewer for a worktree's panel-rust/ui/*.slint tree --
# `slint-viewer --auto-reload`, no C++/cmake build, no acpx-server, no
# real backend. Generalizes the one-off scripts/launch_slint_vnc_ui_
# polish_a.sh script (hardcoded display :111 / port 5911 / one dedicated
# Xvnc for a single worktree) into a per-worktree command that reuses the
# SAME layered infrastructure vnc_worktree.sh/vnc-up already go through --
# vnc_shared_init.sh's `init` (one shared Xvnc across every worktree, not
# a new one per call) + `workspace-ensure` (this worktree's already-
# assigned virtual-desktop slot -- the same one `vnc-up` would place
# Snapflow on; calling `workspace-ensure` again for the same worktree is
# idempotent, it does not allocate a second slot) + `workspace-place`
# (move this specific window onto that slot). Deliberately does NOT call
# port_registry.sh's `vnc-start` directly (that spins up an independent,
# unshared Xvnc with its own display/port -- correct for a truly
# standalone use, wrong here, where the whole point is landing on the
# *same* VNC session and workspace an operator already has open for this
# worktree).
#
# Kept as a standalone script (not inlined into the Makefile recipe)
# following this repo's own established pattern -- every other non-
# trivial dev-infrastructure operation here (vnc_worktree.sh,
# vnc_shared_init.sh, cargo_build_quiet.sh) is a script the Makefile
# calls, not multi-line inline recipe shell. A real, reproduced reason
# to keep that pattern: an equivalent inline `@shared_env="$$(... init)"`
# recipe line reliably hung until an outer timeout killed it when run
# through this Makefile's nested `@+$(MAKE) -f ... "$@"` submake proxy
# chain (dev.make -> development/Makefile -> development/docker/
# Makefile) -- almost certainly GNU Make's jobserver fd inheritance
# (`+` preserves jobserver-auth fds into the recipe shell; this repo's
# own cmake build logs already show "failed to connect to jobserver...
# cannot open file descriptor 3" warnings from the same submake nesting)
# interacting badly with a backgrounded `&` child inside that recipe
# shell. A plain top-level script invocation (`@"$(SCRIPT)" start ...`,
# a normal one-line recipe with no inline command substitution/
# backgrounding of its own) does not hit this.
#
# Usage:
#   slint_hot_reload.sh start <worktree-dir>
#   slint_hot_reload.sh stop  <worktree-dir>

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SHARED_VNC="$SCRIPT_DIR/vnc_shared_init.sh"
STATE_BASE="${VNC_STATE_BASE:-${HOME}/.snapflow/state}"
export VNC_SHARED_ROOT="${VNC_SHARED_ROOT:-$STATE_BASE/snapflow/vnc-shared}"

die() { echo "error: $*" >&2; exit 1; }

worktree_label() {
  git -C "$1" rev-parse --abbrev-ref HEAD 2>/dev/null || basename "$1"
}

state_dir_for() {
  echo "$STATE_BASE/snapflow/slint-hot-reload/$(worktree_label "$1")"
}

cmd_start() {
  local worktree_dir
  worktree_dir="$(cd "${1:?worktree dir required}" && pwd)" || die "no such worktree dir: $1"
  local label state_dir
  label="$(worktree_label "$worktree_dir")"
  state_dir="$(state_dir_for "$worktree_dir")"
  mkdir -p "$state_dir"

  command -v slint-viewer >/dev/null 2>&1 || die "slint-viewer is not on PATH (cargo install slint-viewer)"
  [ -f "$worktree_dir/panel-rust/tests/dev_root.slint.template" ] || die "missing panel-rust/tests/dev_root.slint.template under $worktree_dir"

  # Self-heal, same idea as vnc_worktree.sh's cmd_start: tear down
  # whatever this worktree's own hot-reload viewer previously had
  # running before starting a fresh one. Deliberately does NOT touch the
  # shared workspace registry (unlike a naive cmd_stop call would) --
  # releasing this worktree's workspace slot here, right before
  # re-requesting one a few lines down, is a real race: if any other
  # worktree's session calls workspace-ensure in between, it can claim
  # the just-freed low slot number, so THIS worktree's re-request lands
  # on a different (often higher, never-before-used) slot instead of the
  # stable one it actually already owns -- confirmed live: this exact
  # bug produced an empty workspaces.tsv entry for this worktree after a
  # supposedly-successful start. workspace-ensure is already idempotent
  # (an existing (thread_id) reservation is returned as-is, see
  # vnc_shared_init.sh), so simply never releasing it here is sufficient
  # for repeated `start` calls to keep landing on the same slot.
  if [ -f "$state_dir/viewer.pid" ]; then
    kill -9 "$(cat "$state_dir/viewer.pid")" 2>/dev/null || true
    rm -f "$state_dir/viewer.pid"
  fi
  pkill -9 -f "slint-viewer.*$state_dir/dev_root.slint" 2>/dev/null || true

  local shared_env vnc_port vnc_display workspace_id
  shared_env="$("$SHARED_VNC" init)"
  vnc_port="$(printf '%s\n' "$shared_env" | awk -F= '$1 == "vnc_port" { print $2 }')"
  vnc_display="$(printf '%s\n' "$shared_env" | awk -F= '$1 == "display" { print $2 }')"
  workspace_id="$("$SHARED_VNC" workspace-ensure "$worktree_dir")"

  sed "s#__CHATPANEL_SLINT__#$worktree_dir/panel-rust/ui/app.slint#" \
    "$worktree_dir/panel-rust/tests/dev_root.slint.template" \
    > "$state_dir/dev_root.slint"

  DISPLAY="$vnc_display" LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=softpipe QT_X11_NO_MITSHM=1 \
    slint-viewer --auto-reload "$state_dir/dev_root.slint" \
    > "$state_dir/slint-viewer.log" 2>&1 &
  local viewer_pid=$!
  disown
  echo "$viewer_pid" > "$state_dir/viewer.pid"

  sleep 1
  DISPLAY="$vnc_display" "$SHARED_VNC" workspace-place "$worktree_dir" "$viewer_pid" "$workspace_id" \
    || echo "warning: could not place slint-viewer pid $viewer_pid on workspace $workspace_id" >&2

  echo "=== Slint hot-reload viewer is live (shared VNC) ==="
  echo "worktree : $label"
  echo "vnc      : localhost:$vnc_port  (no password)"
  echo "display  : $vnc_display"
  echo "workspace: $workspace_id  (same slot vnc-up would use for this worktree)"
  echo "slint    : $worktree_dir/panel-rust/ui/app.slint  (edit + save to hot-reload)"
  echo "log      : $state_dir/slint-viewer.log"
  echo "stop with: $0 stop $worktree_dir"
  echo "======================================="
}

cmd_stop() {
  local worktree_dir
  worktree_dir="$(cd "${1:?worktree dir required}" && pwd)" || die "no such worktree dir: $1"
  local label state_dir
  label="$(worktree_label "$worktree_dir")"
  state_dir="$(state_dir_for "$worktree_dir")"

  if [ -f "$state_dir/viewer.pid" ]; then
    kill -9 "$(cat "$state_dir/viewer.pid")" 2>/dev/null || true
    rm -f "$state_dir/viewer.pid"
  fi
  pkill -9 -f "slint-viewer.*$state_dir/dev_root.slint" 2>/dev/null || true
  "$SHARED_VNC" workspace-release "$worktree_dir" 2>/dev/null || true
  echo "stopped slint hot-reload viewer for $label (shared Xvnc left running for other worktrees)"
}

case "${1:-}" in
  start) shift; cmd_start "$@" ;;
  stop)  shift; cmd_stop "$@" ;;
  *) die "usage: $0 {start|stop} <worktree-dir>" ;;
esac
