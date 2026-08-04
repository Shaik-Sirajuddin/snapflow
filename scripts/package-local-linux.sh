#!/usr/bin/env bash
# Assemble the already-built local Linux components into the same install
# bundle shape consumed by scripts/install.sh. This does not start anything
# and never touches ~/.snapshotd or any other user state.

set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/package-local-linux.sh [options]

Options:
  --output DIR       output directory (default: ./dist)
  --version VALUE    bundle version (default: local-YYYYMMDD-HHMMSS)
  --gui PATH         built Snapflow executable
  --app DIR          packaged Snapflow.app runtime directory
  --daemon PATH      snapflowd executable
  --acpx PATH        acpx-server executable
  --help             show this help
EOF
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
output_dir="$repo_root/dist"
version="local-$(date +%Y%m%d-%H%M%S)"
gui_bin="$repo_root/shotcut-rebrand/build-local/src/snapflow"
app_dir="$repo_root/shotcut-rebrand/scripts/Snapflow/Snapflow.app"
daemon_bin="$repo_root/snapshotd/snapshotd"
acpx_bin="$repo_root/acpx/target/release/acpx-server"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --output) [ "$#" -ge 2 ] || { echo "error: --output needs a directory" >&2; exit 2; }; output_dir="$2"; shift 2 ;;
    --version) [ "$#" -ge 2 ] || { echo "error: --version needs a value" >&2; exit 2; }; version="$2"; shift 2 ;;
    --gui) [ "$#" -ge 2 ] || { echo "error: --gui needs a path" >&2; exit 2; }; gui_bin="$2"; shift 2 ;;
    --app) [ "$#" -ge 2 ] || { echo "error: --app needs a directory" >&2; exit 2; }; app_dir="$2"; shift 2 ;;
    --daemon) [ "$#" -ge 2 ] || { echo "error: --daemon needs a path" >&2; exit 2; }; daemon_bin="$2"; shift 2 ;;
    --acpx) [ "$#" -ge 2 ] || { echo "error: --acpx needs a path" >&2; exit 2; }; acpx_bin="$2"; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) echo "error: unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

require_file() {
  [ -f "$1" ] && [ -x "$1" ] || { echo "error: executable not found: $1" >&2; exit 1; }
}

require_file "$gui_bin"
require_file "$daemon_bin"
require_file "$acpx_bin"
[ -d "$app_dir" ] || { echo "error: packaged GUI runtime not found: $app_dir" >&2; exit 1; }

# The local CMake build links the GUI against CuteLogger from the sibling
# build directory.  The regular release bundler installs this library into
# Snapflow.app/lib, but a local package must do that explicitly or the binary
# carries a host-only RUNPATH and fails on another machine.  Keep this lookup
# relative to --gui so custom build directories continue to work.
gui_dir="$(cd "$(dirname "$gui_bin")" && pwd)"
cute_logger_bin="$gui_dir/../CuteLogger/libCuteLogger.so"
[ -f "$cute_logger_bin" ] || {
  echo "error: local GUI dependency not found: $cute_logger_bin" >&2
  echo "       build CuteLogger alongside the GUI or pass a release GUI binary" >&2
  exit 1
}

mkdir -p "$output_dir"
stage="$(mktemp -d "${TMPDIR:-/tmp}/snapflow-local-bundle.XXXXXX")"
cleanup() { rm -rf "$stage"; }
trap cleanup EXIT

bundle_name="snapflow-linux-x86_64-$version"
bundle="$stage/$bundle_name"
mkdir -p "$bundle/bin" "$bundle/share/applications" "$bundle/share/icons" "$bundle/scripts/lib"

echo "==> copying packaged GUI runtime"
cp -a "$app_dir" "$bundle/Snapflow.app"
install -Dm755 "$gui_bin" "$bundle/Snapflow.app/bin/snapflow"
install -Dm755 "$acpx_bin" "$bundle/Snapflow.app/bin/acpx-server"
install -Dm755 "$cute_logger_bin" "$bundle/Snapflow.app/lib/libCuteLogger.so"
install -Dm755 "$daemon_bin" "$bundle/bin/snapflowd"
install -Dm755 "$repo_root/scripts/lib/acp-node-runtime.sh" "$bundle/scripts/lib/acp-node-runtime.sh"

# Keep the wrapper contract used by the release bundle: the installer links
# this script, not the raw Qt binary, so the bundled media/Qt libraries are
# found without requiring LD_LIBRARY_PATH from the user's shell.
cat > "$bundle/Snapflow.app/snapflow" <<'EOF'
#!/bin/sh
set -eu
CURRENT_DIR=$(readlink -f "$0")
INSTALL_DIR=$(dirname "$CURRENT_DIR")
export LD_LIBRARY_PATH="$INSTALL_DIR/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export MLT_REPOSITORY="$INSTALL_DIR/lib/mlt-7"
export MLT_DATA="$INSTALL_DIR/share/mlt-7"
export MLT_PROFILES_PATH="$INSTALL_DIR/share/mlt-7/profiles"
export MLT_MOVIT_PATH="$INSTALL_DIR/share/movit"
export FREI0R_PATH="$INSTALL_DIR/lib/frei0r-1"
export LADSPA_PATH="$INSTALL_DIR/lib/ladspa"
export PYTHONHOME="$INSTALL_DIR"
export QT_PLUGIN_PATH="$INSTALL_DIR/lib/qt6"
export QML2_IMPORT_PATH="$INSTALL_DIR/lib/qml"
cd "$INSTALL_DIR"
exec bin/snapflow "$@"
EOF
chmod 755 "$bundle/Snapflow.app/snapflow"

install -Dm644 "$repo_root/shotcut-rebrand/packaging/linux/org.snapflow.Snapflow.desktop" \
  "$bundle/share/applications/org.snapflow.Snapflow.desktop"
install -Dm644 "$repo_root/shotcut-rebrand/packaging/linux/icons/128x128/org.snapflow.Snapflow.png" \
  "$bundle/share/icons/org.snapflow.Snapflow.png"

archive="$output_dir/$bundle_name.tar.gz"
echo "==> creating $archive"
tar -czf "$archive" -C "$stage" --owner=0 --group=0 --numeric-owner "$bundle_name"
sha256sum "$archive" > "$archive.sha256"
echo "==> bundle ready"
echo "    archive: $archive"
echo "    checksum: $archive.sha256"
