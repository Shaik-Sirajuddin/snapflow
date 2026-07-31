#!/usr/bin/env bash
# One command to run a real snapflow editor + acpx-server against a
# specific git worktree's Rust sources, reachable over VNC -- start,
# rebuild, clean, status.
#
# Why this exists (real incident, 2026-07-26): a session hand-editing
# shotcut-rebrand/CMakeLists.txt's two hardcoded corrosion_import_crate
# MANIFEST_PATHs to point at whichever worktree it wanted to test, then
# manually rebuilding, manually picking VNC/gateway ports, and manually
# writing a run.sh into /tmp -- every step ad hoc, uncommitted, and
# unsafe the moment two sessions did it against two different worktrees
# concurrently (whichever rebuilt last silently won, the other's "live"
# VNC session kept serving a binary built from someone else's source).
#
# What this fixes:
#   - CMakeLists.txt's manifest paths are now CACHE FILEPATH variables
#     (see shotcut-rebrand's own commit), so repointing is a `cmake -D`
#     reconfigure, not a file edit racing every other session touching
#     the same shared checkout.
#   - VNC display/port allocation goes through port_registry.sh's
#     existing vnc-start/vnc-stop (OS-assigned free ports, per-worktree
#     tagged, reuse-if-alive) -- never guessed/hardcoded.
#   - The gateway (acpx-server) port goes through the same registry's
#     `reserve`, same collision-avoidance.
#   - State (logs, sqlite, appdata) lives in a per-worktree dir under
#     /tmp, never shared between worktrees.
#
# Known limitation (v1, by design -- see below): shotcut-rebrand's C++
# build-local is ONE shared build directory. Only one worktree's Rust
# sources can be linked into the on-disk snapflow binary at a time.
# `start`/`rebuild` for worktree B while worktree A's instance is still
# running will repoint + relink the shared binary out from under A's
# already-running process (the running process keeps executing its old
# in-memory copy fine, per normal Unix semantics -- but the *file* on
# disk changes, so if A's process is later restarted without a fresh
# `start` first, it'll pick up B's build). This is safe for sequential
# testing (the common case) but NOT true concurrent multi-worktree GUI
# testing. That needs a separate build-local per worktree -- real, but
# expensive: shotcut-rebrand's C++ side has no ccache configured here, so
# a second from-scratch build-local is a genuinely slow cold C++ compile,
# not the cheap incremental relink a shared build-local gets when only
# the linked Rust crates changed. Deliberately not built into v1 --
# revisit if concurrent multi-worktree GUI testing becomes a real need.
# acpx-server has no such limitation: each worktree gets its OWN
# acpx-server binary (built from that worktree's own acpx/ checkout),
# so gateway processes across worktrees are always genuinely concurrent.
#
# Usage:
#   vnc_worktree.sh start   <worktree-dir> [agent-id]
#   vnc_worktree.sh rebuild <worktree-dir> [agent-id]
#   vnc_worktree.sh clean   <worktree-dir>
#   vnc_worktree.sh status
#
# <worktree-dir> is the path to the git worktree (e.g.
# .../.claude/worktrees/main-vnc-view-test) whose panel-rust/sap-rust/
# acpx sources should be built and served. agent-id defaults to
# codex-acp (see host_vnc_dev.sh's identical convention -- never the
# rui-mock-agent stub; only automated test harnesses use that).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
REG="$REPO_ROOT/memory/team/reserved/port_registry.sh"
SHARED_VNC="$SCRIPT_DIR/vnc_shared_init.sh"
STATE_BASE="${VNC_STATE_BASE:-${HOME}/.snapflow/state}"
VNC_SHARED_ROOT="${VNC_SHARED_ROOT:-$STATE_BASE/snapflow/vnc-shared}"
PORT_REGISTRY_FILE="${PORT_REGISTRY_FILE:-$VNC_SHARED_ROOT/ports.json}"
export PORT_REGISTRY_FILE
SHOTCUT_DIR="$REPO_ROOT/shotcut-rebrand"
BUILD_DIR="$SHOTCUT_DIR/build-local"
SNAPFLOW_BIN="${SNAPFLOW_BIN_OVERRIDE:-$BUILD_DIR/src/snapflow}"
ACTIVE_MARKER="$BUILD_DIR/.vnc_worktree_active"
SOURCE_MARKER="$BUILD_DIR/.vnc_worktree_cmake_source"

# Worktrees may carry memory as an unpopulated gitlink. Reuse the main
# checkout's shared harness helpers in that case; all daemon/acpx state still
# remains worktree-scoped under /tmp.
if [ ! -x "$REG" ]; then
    COMMON_GIT_DIR="$(git -C "$REPO_ROOT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)"
    if [ -n "$COMMON_GIT_DIR" ]; then
        COMMON_ROOT="$(cd "$COMMON_GIT_DIR/.." && pwd)"
        REG="$COMMON_ROOT/memory/team/reserved/port_registry.sh"
    fi
fi

die() { echo "error: $*" >&2; exit 1; }

worktree_label() {
    # branch name if available, else the directory basename -- matches
    # the label already used in ports.json (e.g.
    # "worktree-main-vnc-view-test").
    git -C "$1" rev-parse --abbrev-ref HEAD 2>/dev/null || basename "$1"
}

state_dir_for() {
    echo "/tmp/vnc-worktree-$(worktree_label "$1")"
}

ensure_shared_build_points_at() {
    local worktree_dir="$1"
    local sap_manifest="$worktree_dir/sap-rust/Cargo.toml"
    local panel_manifest="$worktree_dir/panel-rust/Cargo.toml"
    [ -f "$sap_manifest" ] || die "no sap-rust/Cargo.toml under $worktree_dir"
    [ -f "$panel_manifest" ] || die "no panel-rust/Cargo.toml under $worktree_dir"

    local current=""
    local current_source=""
    local cmake_source="$SHOTCUT_DIR"
    if [ -f "$worktree_dir/shotcut/CMakeLists.txt" ]; then
        cmake_source="$worktree_dir/shotcut"
    fi
    [ -f "$ACTIVE_MARKER" ] && current="$(cat "$ACTIVE_MARKER")"
    [ -f "$SOURCE_MARKER" ] && current_source="$(cat "$SOURCE_MARKER")"
    local force_reconfigure=0
    if [ "$current" != "$worktree_dir" ] || [ "$current_source" != "$cmake_source" ]; then
        echo "==> repointing shared shotcut-rebrand build: $current -> $worktree_dir"
        echo "==> C++ source: ${current_source:-none} -> $cmake_source"
        force_reconfigure=1
    fi

    if [ "$force_reconfigure" -eq 1 ] || [ "${2:-}" = "--force" ]; then
        # Real bug found 2026-07-26: corrosion's cargo --target-dir is
        # ONE shared path (build-local/cargo/build) regardless of which
        # worktree's Cargo.toml is pointed at. Cargo's fingerprint cache
        # keys on crate name+version, not manifest path -- so switching
        # worktrees without clearing this reused a DIFFERENT worktree's
        # already-built `panel-rust` build-script binary (confirmed: it
        # failed demanding a translations/ dir that only one worktree's
        # build.rs even references, while building a worktree whose own
        # build.rs has no such call at all). Wiping this on every switch
        # costs a full cargo rebuild instead of an incremental one, but a
        # stale cross-worktree build-script binary is a correctness bug,
        # not just a slowdown -- worth the cost.
        # Concurrent cargo/cc can leave race residue; don't abort the whole
        # start path on a partial rm (set -e). Retry once, then proceed.
        rm -rf "$BUILD_DIR" 2>/dev/null || true
        mkdir -p "$BUILD_DIR"
        # VNC_CORROSION_SOURCE_DIR: opt-in escape hatch for offline hosts --
        # every `rm -rf "$BUILD_DIR"` above forces FetchContent to re-clone
        # corrosion-rs/corrosion from GitHub on the next configure, which
        # fails outright with no network path. Pointing FETCHCONTENT_SOURCE_
        # DIR_CORROSION at an already-populated `_deps/corrosion-src` (e.g.
        # copied from another worktree's build dir) makes FetchContent use
        # it directly and skip the git step entirely.
        local -a extra_cmake_args=()
        if [ -n "${VNC_CORROSION_SOURCE_DIR:-}" ]; then
            extra_cmake_args+=(
                "-DFETCHCONTENT_SOURCE_DIR_CORROSION=$VNC_CORROSION_SOURCE_DIR"
                "-DFETCHCONTENT_UPDATES_DISCONNECTED_CORROSION=ON"
            )
        fi
        cmake -S "$cmake_source" -B "$BUILD_DIR" \
            -DSAP_RUST_MANIFEST_PATH="$sap_manifest" \
            -DPANEL_RUST_MANIFEST_PATH="$panel_manifest" \
            "${extra_cmake_args[@]}" \
            >/tmp/vnc-worktree-cmake-configure.log 2>&1 \
            || die "cmake reconfigure failed, see /tmp/vnc-worktree-cmake-configure.log"
        echo "$worktree_dir" > "$ACTIVE_MARKER"
        echo "$cmake_source" > "$SOURCE_MARKER"
    fi
}

build_snapflow() {
    echo "==> building snapflow (incremental; only rebuilds what changed)..."
    cmake --build "$BUILD_DIR" --target snapflow -j"$(nproc)" \
        >/tmp/vnc-worktree-cmake-build.log 2>&1 \
        || die "snapflow build failed, see /tmp/vnc-worktree-cmake-build.log"
}

build_acpx_server() {
    local worktree_dir="$1"
    echo "==> building acpx-server for this worktree..."
    ( cd "$worktree_dir/acpx" && cargo build -p acpx-server >/tmp/vnc-worktree-acpx-build.log 2>&1 ) \
        || die "acpx-server build failed, see /tmp/vnc-worktree-acpx-build.log"
}

cmd_start() {
    # FORCE_REBUILD is set by cmd_rebuild (never passed positionally --
    # keeps `start <dir> <agent-id>` and `rebuild <dir> <agent-id>`
    # symmetric instead of one of them needing a --force in the middle).
    local worktree_dir agent_id="${2:-codex-acp}"
    worktree_dir="$(cd "$1" && pwd)" || die "no such worktree dir: $1"
    local label state_dir
    label="$(worktree_label "$worktree_dir")"
    state_dir="$(state_dir_for "$worktree_dir")"

    # Self-heal: every `start` first tears down whatever this worktree
    # previously had running, so re-running `up` (or `up` after a crash)
    # never accumulates orphaned snapshotd/acpx/shotcut processes from an
    # earlier launch of the SAME worktree. cmd_clean is a no-op when
    # nothing is running.
    cmd_clean "$worktree_dir" || true

    for bin in cmake jq curl; do
        command -v "$bin" >/dev/null 2>&1 || die "required executable is unavailable: $bin"
    done

    if [ "${VNC_SKIP_BUILD:-0}" = "1" ]; then
        [ -x "$SNAPFLOW_BIN" ] || die "missing snapflow binary: $SNAPFLOW_BIN"
        [ -x "$worktree_dir/acpx/target/debug/acpx-server" ] || die "missing acpx-server binary under $worktree_dir/acpx/target/debug"
    else
        if [ "${FORCE_REBUILD:-0}" = "1" ]; then
            ensure_shared_build_points_at "$worktree_dir" --force
        else
            ensure_shared_build_points_at "$worktree_dir"
        fi
        build_snapflow
        build_acpx_server "$worktree_dir"
    fi

    echo "==> reserving VNC display + gateway port for $label..."
    local vnc_out vnc_port vnc_display gateway_port workspace_id=""
    if [ "${VNC_SHARED:-0}" = "1" ]; then
        [ -x "$SHARED_VNC" ] || die "shared VNC controller is not executable: $SHARED_VNC"
        local shared_env
        shared_env="$("$SHARED_VNC" init "$worktree_dir")"
        vnc_port="$(printf '%s\n' "$shared_env" | awk -F= '$1 == "vnc_port" { print $2 }')"
        vnc_display="$(printf '%s\n' "$shared_env" | awk -F= '$1 == "display" { print $2 }')"
        workspace_id="$("$SHARED_VNC" workspace-ensure "$worktree_dir")"
    else
        vnc_out="$("$REG" vnc-start "$label" "vnc_worktree.sh instance")"
        vnc_port="$(echo "$vnc_out" | cut -d' ' -f1)"
        vnc_display="$(echo "$vnc_out" | cut -d' ' -f2)"
    fi
    gateway_port="$("$REG" reserve "$label" "acpx-gateway vnc_worktree.sh")"
    local admin_port
    admin_port="$("$REG" reserve "$label" "acpx-admin vnc_worktree.sh")"

    # Opt-in Slint MCP server (i_slint_backend_testing::mcp_server) on the
    # real-backend snapflow this script launches -- previously the only way
    # to drive this instance's UI via MCP was to hand-kill and relaunch
    # snapflow with SLINT_MCP_PORT bolted on manually outside this script
    # (a real, repeated "temporary test" workaround). `VNC_MCP_PORT=1`
    # reserves a real port through the same registry gateway_port/mcp_addr
    # already use (never collides with a concurrent worktree's instance);
    # `VNC_MCP_PORT=<port>` pins a specific one. Unset/0 (the default)
    # leaves this real-backend instance exactly as before -- no MCP server,
    # matching production dev-harness behavior.
    local mcp_port=""
    if [ -n "${VNC_MCP_PORT:-}" ] && [ "${VNC_MCP_PORT}" != "0" ]; then
        if [ "${VNC_MCP_PORT}" = "1" ]; then
            mcp_port="$("$REG" reserve "$label" "slint mcp vnc_worktree.sh")"
        else
            mcp_port="$VNC_MCP_PORT"
        fi
    fi

    mkdir -p "$state_dir"/{acpx,panel,shotcut}
    # snapshotd owns the acpx child and its provisioning file. Ensure a
    # worktree-scoped daemon via snapshotd_test_instance.ensure_daemon so the
    # live daemon MCP URL and the acpx central registry are initialized before
    # the panel connects.
    echo "==> ensuring snapshotd serve for $label..."
    # The selected worktree owns the GUI lifecycle protocol and its matching
    # daemon implementation. Running the root checkout's snapshotd here can
    # silently reject newer registerExternalInstance/heartbeat calls from the
    # worktree's panel-rust, leaving daemon.list empty even though the GUI is
    # alive. Build and run the daemon from the same checkout as Snapflow.
    local snapshotd_bin="$worktree_dir/snapshotd/snapshotd"
    local snap_bin_path="$SNAPFLOW_BIN"
    [ -x "$snapshotd_bin" ] || die "no snapshotd binary at $snapshotd_bin (go build ./cmd/snapshotd)"
    local mcp_addr
    # ensure_daemon/build_freshness print progress on stdout — capture only
    # the last line (the pure addr) so SNAPSHOTD_MCP_SSE_ADDR stays clean.
    mcp_addr="$(
        python3 - <<PY
import sys
from pathlib import Path
sys.path.insert(0, "$SCRIPT_DIR")
sys.path.insert(0, str(Path("$REPO_ROOT") / "memory" / "team" / "testing"))
from snapshotd_test_instance import ensure_daemon, load_instance
import os
scratch = Path("$state_dir") / "snapshotd-scratch"
scratch.mkdir(parents=True, exist_ok=True)
# The daemon owns the gateway for this worktree. Give it the exact worktree
# acpx binary, reserved gateway bind, and worktree-scoped config path.
os.environ["SNAPSHOTD_ACPX_ENABLED"] = "true"
os.environ["SNAPSHOTD_ACPX_BIN"] = "$worktree_dir/acpx/target/debug/acpx-server"
os.environ["SNAPSHOTD_ACPX_HTTP_BIND"] = "0.0.0.0:$gateway_port"
os.environ["SNAPSHOTD_ACPX_ADMIN_BIND"] = "127.0.0.1:$admin_port"
os.environ["SNAPSHOTD_ACPX_CONFIG"] = "$state_dir/snapshotd-scratch/snapshotd-home/acpx-config.json"
# Stable session id so ensure_daemon can reuse across VNC restarts.
os.environ.setdefault(
    "CLAUDE_CODE_SESSION_ID",
    "vnc-worktree-$label",
)
# Route library chatter to stderr so bash capture stays clean.
_real_stdout = sys.stdout
sys.stdout = sys.stderr
try:
    mcp_url, reused = ensure_daemon(
        scratch,
        "$snapshotd_bin",
        "$snap_bin_path",
        worktree="$label",
        auto_rebuild=True,
    )
finally:
    sys.stdout = _real_stdout
inst = load_instance(scratch) or {}
port = inst.get("mcp_port")
if not port:
    raise SystemExit(f"ensure_daemon returned no port: {mcp_url=} {reused=}")
print(f"reused={reused} mcp_url={mcp_url}", file=sys.stderr)
print(f"127.0.0.1:{port}")
PY
    )" || die "snapshotd ensure_daemon failed"
    mcp_addr="$(printf '%s\n' "$mcp_addr" | tail -n1 | tr -d '[:space:]')"
    [[ "$mcp_addr" == 127.0.0.1:* ]] || die "bad SNAPSHOTD_MCP_SSE_ADDR from ensure_daemon: '$mcp_addr'"
    # The panel discovers the authoritative MCP endpoint through the daemon
    # control socket, not by guessing the configured TCP address. Propagate
    # the worktree-scoped daemon home so systemd/manual GUI launches probe
    # the same control.sock that ensure_daemon started.
    local snapshotd_home="$state_dir/snapshotd-scratch/snapshotd-home"
    mkdir -p "$snapshotd_home/run"
    # The embedded SAP endpoint must be present before panel-rust registers
    # the GUI with snapshotd; otherwise MCP can see the project path but has
    # no routable GUI socket and falls back to launching headless.
    local sap_socket="$snapshotd_home/run/g.sap.sock"
    local sap_token
    sap_token="$(openssl rand -hex 16 2>/dev/null || date +%s%N)"
    echo "    SNAPSHOTD_MCP_SSE_ADDR=$mcp_addr"
    echo "    SNAPSHOTD_HOME=$snapshotd_home"

    echo "==> waiting for daemon-managed acpx-server..."
    local acpx_config="$snapshotd_home/acpx-config.json"
    local acpx_pid_file="$snapshotd_home/acpx-server.pid"
    local server_pid=""
    local fifo_keeper_pid=""
    for _ in $(seq 1 100); do
        if [ -s "$acpx_pid_file" ]; then
            server_pid="$(tr -d '[:space:]' <"$acpx_pid_file")"
            kill -0 "$server_pid" 2>/dev/null && break
            server_pid=""
        fi
        sleep 0.1
    done
    [ -n "$server_pid" ] || die "daemon-managed acpx-server pid file was not created"
    "$REG" update-pid "$gateway_port" "$server_pid" || true

    for _ in $(seq 1 100); do
        curl --fail --silent "http://127.0.0.1:$gateway_port/health" >/dev/null 2>&1 && break
        sleep 0.1
    done
    curl --fail --silent "http://127.0.0.1:$gateway_port/health" >/dev/null \
        || die "daemon-managed acpx-server never became healthy, see $state_dir/snapshotd-scratch/snapshotd_stdout.log"

    echo "==> starting snapflow on display $vnc_display..."
    local project_args=()
    if [ -n "${VNC_PROJECT_PATH:-}" ]; then
        project_args+=("$VNC_PROJECT_PATH")
    fi
    local snapflow_window_args=(--noupgrade)
    # A shared VNC workspace needs to leave room for Fluxbox's workspace
    # toolbar. Fullscreen windows cover that toolbar, making workspace 0..31
    # impossible to select with the mouse.
    if [ "${VNC_SHARED:-0}" != "1" ]; then
        snapflow_window_args+=(--fullscreen)
    fi
    local -a snapflow_env=(
        DISPLAY="$vnc_display"
        QT_QPA_PLATFORM=xcb
        QT_X11_NO_MITSHM=1
        LIBGL_ALWAYS_SOFTWARE=1
        QT_QUICK_BACKEND=software
        QSG_RENDER_LOOP=basic
        RUI_ACP_CACHE_DIR="$state_dir/panel"
        RUI_PANEL_INPUT_TRACE="${RUI_PANEL_INPUT_TRACE:-}"
        # Development VNC harness only: route every persisted provider profile
        # through this worktree gateway and prevent panel-rust from spawning a
        # competing internal acpx-server. Production config does not export
        # provider-specific localhost URLs.
        RUI_ACPX_DEFAULT_URL="http://127.0.0.1:$gateway_port"
        RUI_ACPX_ADMIN_URL="http://127.0.0.1:$admin_port"
        RUI_ACPX_NO_AUTOSPAWN=1
        SNAPSHOTD_HOME="$snapshotd_home"
        SNAPSHOTD_MCP_SSE_ADDR="$mcp_addr"
        SNAPSHOT_SAP_SOCKET="$sap_socket"
        SNAPSHOT_SAP_TOKEN="$sap_token"
        RUI_SNAPSHOTD_BIN="$snapshotd_bin"
    )
    if [ -n "$mcp_port" ]; then
        snapflow_env+=(SLINT_MCP_PORT="$mcp_port")
    fi
    env "${snapflow_env[@]}" \
        setsid nohup "$SNAPFLOW_BIN" --appdata "$state_dir/shotcut" "${snapflow_window_args[@]}" "${project_args[@]}" \
        >"$state_dir/shotcut.stdout.log" 2>"$state_dir/shotcut.stderr.log" &
    local shotcut_pid=$!

    if [ "${VNC_SHARED:-0}" = "1" ]; then
        if ! DISPLAY="$vnc_display" "$SHARED_VNC" workspace-place \
            "$worktree_dir" "$shotcut_pid" "$workspace_id"; then
            # Do not leave an untracked live editor behind when placement
            # fails.  The old path exited before connect.env was written,
            # making the next clean unable to discover this process.
            kill_process_group "$shotcut_pid"
            kill_process_group "$server_pid"
            kill_process_group "$fifo_keeper_pid"
            python3 "$SNAPSHOTD_TEST_INSTANCE" teardown-worktree "$label" \
                >/dev/null 2>&1 || true
            "$SHARED_VNC" workspace-release "$worktree_dir" \
                >/dev/null 2>&1 || true
            "$REG" release "$admin_port" >/dev/null 2>&1 || true
            "$REG" release "$gateway_port" >/dev/null 2>&1 || true
            die "could not place Snapflow pid $shotcut_pid on workspace $workspace_id"
        fi
    fi

    cat > "$state_dir/connect.env" <<EOF
worktree=$label
worktree_dir=$worktree_dir
vnc_port=$vnc_port
display=$vnc_display
gateway_port=$gateway_port
admin_port=$admin_port
state_dir=$state_dir
vnc_connect=localhost:$vnc_port
workspace=$workspace_id
acpx_bin=$worktree_dir/acpx/target/debug/acpx-server
acpx_config=$acpx_config
snapflow_bin=$SNAPFLOW_BIN
snapshotd_mcp_sse_addr=$mcp_addr
slint_mcp_port=$mcp_port
built_at=$(date -Iseconds)
git_head=$(git -C "$worktree_dir" rev-parse --short HEAD 2>/dev/null || echo unknown)
server_pid=$server_pid
shotcut_pid=$shotcut_pid
fifo_keeper_pid=$fifo_keeper_pid
EOF

    echo ""
    echo "=== ready ==="
    echo "worktree : $label ($(git -C "$worktree_dir" rev-parse --short HEAD 2>/dev/null))"
    echo "vnc      : localhost:$vnc_port  (no password)"
    echo "display  : $vnc_display"
    echo "ports    : $PORT_REGISTRY_FILE"
    if [ "${VNC_SHARED:-0}" = "1" ]; then
        echo "workspace: $workspace_id (worktree=$label)"
        echo "registry : ${VNC_SHARED_ROOT}/workspaces.tsv"
    fi
    echo "gateway  : http://0.0.0.0:$gateway_port"
    if [ -n "$mcp_port" ]; then
        echo "mcp      : http://127.0.0.1:$mcp_port/mcp  (VNC_MCP_PORT was set -- real backend, real UI, MCP-drivable)"
    else
        echo "mcp      : disabled (set VNC_MCP_PORT=1 to enable the Slint MCP server on this real-backend instance)"
    fi
    echo "state    : $state_dir"
    echo "stop with: $SCRIPT_DIR/vnc_worktree.sh clean $worktree_dir"
    echo "============="
}

cmd_rebuild() {
    FORCE_REBUILD=1 cmd_start "$1" "${2:-codex-acp}"
}

SNAPSHOTD_TEST_INSTANCE="$REPO_ROOT/memory/team/testing/snapshotd_test_instance.py"
if [ ! -f "$SNAPSHOTD_TEST_INSTANCE" ] && declare -p COMMON_ROOT >/dev/null 2>&1; then
    SNAPSHOTD_TEST_INSTANCE="$COMMON_ROOT/memory/team/testing/snapshotd_test_instance.py"
fi
CLEAN_LOG_DIR="$STATE_BASE/snapflow/vnc-logs"
mkdir -p "$CLEAN_LOG_DIR" 2>/dev/null || true

# Runs "$@", capturing its output to $CLEAN_LOG_DIR/<name>.log instead of the
# terminal -- cmd_clean is best-effort teardown of several sub-tools
# (snapshotd, port registry, shared-VNC workspace release) whose routine
# chatter isn't worth showing every time; a non-zero exit only gets a
# one-line warning pointing at the log, never aborts the rest of cleanup.
quiet_step() {
    local name="$1"; shift
    local log="$CLEAN_LOG_DIR/$name.log"
    if ! "$@" >"$log" 2>&1; then
        echo "warning: $name did not exit cleanly, see $log" >&2
    fi
}

# Kills $1's whole process group (it was launched via setsid, so it's a
# session/group leader -- a bare `kill $pid` misses children like ACPX's
# spawned sub-processes). TERM first, wait briefly, then KILL any survivor.
kill_process_group() {
    local pid="$1"
    [ -n "$pid" ] || return 0
    kill -0 "$pid" 2>/dev/null || return 0
    kill -TERM -- "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
    for _ in $(seq 1 20); do
        kill -0 "$pid" 2>/dev/null || return 0
        sleep 0.1
    done
    kill -KILL -- "-$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null || true
}

cmd_clean() {
    local worktree_dir label
    worktree_dir="$(cd "$1" && pwd)" || die "no such worktree dir: $1"
    label="$(worktree_label "$worktree_dir")"
    local state_dir
    state_dir="$(state_dir_for "$worktree_dir")"

    # Tear down the snapshotd daemon this worktree started (+ its bundled
    # acpx + any orphaned Snapflow editor children) -- keyed by worktree
    # label, independent of connect.env/state_dir, so it recovers even if
    # those are already gone. Previously never called: the real bug behind
    # leaked snapshotd/editor processes surviving `clean`.
    quiet_step "teardown-worktree-$label" python3 "$SNAPSHOTD_TEST_INSTANCE" teardown-worktree "$label"

    local gateway_port=""
    if [ -f "$state_dir/connect.env" ]; then
        # shellcheck source=/dev/null
        source "$state_dir/connect.env"
        kill_process_group "${shotcut_pid:-}"
        kill_process_group "${server_pid:-}"
        kill_process_group "${fifo_keeper_pid:-}"
    fi

    if [ "${VNC_SHARED:-0}" = "1" ]; then
        quiet_step "workspace-release-$label" "$SHARED_VNC" workspace-release "$worktree_dir"
    else
        quiet_step "vnc-stop-$label" "$REG" vnc-stop "$label"
    fi

    if [ -n "$gateway_port" ]; then
        for _ in $(seq 1 20); do
            curl --fail --silent --max-time 1 "http://127.0.0.1:$gateway_port/health" >/dev/null 2>&1 || break
            sleep 0.2
        done
        curl --fail --silent --max-time 1 "http://127.0.0.1:$gateway_port/health" >/dev/null 2>&1 \
            && echo "warning: gateway port $gateway_port still answering after teardown" >&2
    fi
    if [ -v admin_port ] && [ -n "$admin_port" ]; then
        quiet_step "release-admin-$label" "$REG" release "$admin_port"
    fi
    quiet_step "release-worktree-$label" "$REG" release-worktree "$label"
    rm -rf "$state_dir"
    echo "cleaned $label ($state_dir)"
}

cmd_status() {
    echo "=== active vnc_worktree.sh instances ==="
    local found=0
    for state_dir in /tmp/vnc-worktree-*; do
        [ -f "$state_dir/connect.env" ] || continue
        found=1
        # shellcheck source=/dev/null
        (source "$state_dir/connect.env"
         alive="dead"
         kill -0 "${shotcut_pid:-0}" 2>/dev/null && alive="alive"
         printf "%-45s vnc=localhost:%-6s display=%-5s git=%-8s status=%s\n" \
             "$worktree" "$vnc_port" "$display" "$git_head" "$alive")
    done
    [ "$found" -eq 0 ] && echo "(none)"
    echo ""
    echo "=== shared shotcut-rebrand build currently points at ==="
    [ -f "$ACTIVE_MARKER" ] && cat "$ACTIVE_MARKER" || echo "(never built via this tool)"
    echo ""
    echo "=== full port registry ==="
    "$REG" list
}

case "${1:-}" in
    start)   shift; cmd_start "$@" ;;
    rebuild) shift; cmd_rebuild "$@" ;;
    clean)   shift; cmd_clean "$@" ;;
    status)  shift; cmd_status "$@" ;;
    *) die "usage: $0 {start <worktree-dir> [agent-id]|rebuild <worktree-dir> [agent-id]|clean <worktree-dir>|status}" ;;
esac
