#!/usr/bin/env bash
# Quiet-by-default wrapper for `cargo build` -- prints one "==> name" line on
# start, nothing further on success. VERBOSE=1 streams live output instead.
# Either way, full output is captured to $LOG_DIR/<name>.log; on failure only
# the log path + a grep filter suggestion are printed, never the whole log.
#
# Usage: cargo_build_quiet.sh <name> <crate_dir> [cargo build args...]
set -euo pipefail

name="$1"; shift
crate_dir="$1"; shift

VERBOSE="${VERBOSE:-0}"
LOG_DIR="${DOCKER_STATE_ROOT:-$HOME/.snapflow/state/snapflow/docker}/logs"
mkdir -p "$LOG_DIR"
log="$LOG_DIR/$name.log"

echo "==> $name"
status=0
if [[ "$VERBOSE" == "1" ]]; then
    ( cd "$crate_dir" && cargo build "$@" ) 2>&1 | tee "$log" || status=$?
else
    ( cd "$crate_dir" && cargo build -q "$@" ) >"$log" 2>&1 || status=$?
fi

if [[ "$status" -ne 0 ]]; then
    echo "==> $name FAILED (exit $status)" >&2
    echo "    log: $log" >&2
    echo "    inspect: grep -inE 'error|warning' \"$log\" | tail -n 50" >&2
    exit "$status"
fi
echo "==> $name ok"
