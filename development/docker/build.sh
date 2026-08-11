#!/usr/bin/env bash
set -euo pipefail

: "${REPO_ROOT:?REPO_ROOT is required}"
: "${WORKTREE:?WORKTREE is required}"
: "${BUILD_DIR:?BUILD_DIR is required}"
: "${ACTIVE_MARKER:?ACTIVE_MARKER is required}"

VERBOSE="${VERBOSE:-0}"
LOG_DIR="${DOCKER_STATE_ROOT:-/tmp}/logs"
mkdir -p "$LOG_DIR"

# Quiet by default: captures "$@"'s full output to $LOG_DIR/<name>.log and
# only prints it (and the failing step) on a non-zero exit. VERBOSE=1
# streams live instead (still keeping the log file).
run_step() {
  local name="$1"; shift
  local log="$LOG_DIR/$name.log"
  local status=0
  echo "==> $name"
  if [[ "$VERBOSE" == "1" ]]; then
    "$@" 2>&1 | tee "$log" || status=$?
  else
    "$@" >"$log" 2>&1 || status=$?
  fi
  if [[ "$status" -ne 0 ]]; then
    echo "==> $name FAILED (exit $status)" >&2
    echo "    log: $log" >&2
    echo "    inspect: grep -inE 'error|fatal error|undefined reference' \"$log\" | tail -n 50" >&2
    exit "$status"
  fi
}

worktree="$(realpath "$WORKTREE")"
if [[ -f "$worktree/shotcut-rebrand/CMakeLists.txt" ]]; then
  cmake_source="$worktree/shotcut-rebrand"
elif [[ -f "$worktree/shotcut/CMakeLists.txt" ]]; then
  cmake_source="$worktree/shotcut"
else
  cmake_source="$REPO_ROOT/shotcut-rebrand"
fi
current=""
current_source=""
if [[ -f "$ACTIVE_MARKER" ]]; then
  current="$(cat "$ACTIVE_MARKER")"
fi
if [[ -f "$BUILD_DIR/.docker_shared_build_source" ]]; then
  current_source="$(cat "$BUILD_DIR/.docker_shared_build_source")"
fi

if [[ "$current" != "$worktree" || "$current_source" != "$cmake_source" ]]; then
  echo "==> repointing shared Docker build: ${current:-none} -> $worktree"
  echo "==> C++ source: ${current_source:-none} -> $cmake_source"
  rm -rf "$BUILD_DIR"
  mkdir -p "$BUILD_DIR"
  run_step cmake-configure cmake -S "$cmake_source" -B "$BUILD_DIR" \
    -DSAP_RUST_MANIFEST_PATH="$worktree/sap-rust/Cargo.toml" \
    -DPANEL_RUST_MANIFEST_PATH="$worktree/panel-rust/Cargo.toml"
  printf '%s\n' "$worktree" > "$ACTIVE_MARKER"
  printf '%s\n' "$cmake_source" > "$BUILD_DIR/.docker_shared_build_source"
fi

run_step cmake-build-snapflow cmake --build "$BUILD_DIR" --target snapflow -j"$(nproc)"

# The VNC runtime also needs the worktree-local gateway. Build it in the same
# Docker invocation so vnc-up can launch the already-built binaries without a
# second host-side Rust build.
if [[ -f "$worktree/acpx/Cargo.toml" ]]; then
  (cd "$worktree/acpx" && run_step cargo-build-acpx-server cargo build -p acpx-server)
fi
