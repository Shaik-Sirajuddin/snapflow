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
# Upstream's Linux bundle ships the MLT CLI beside the GUI. Snapflow's real
# export path shells out to that binary, so fail while packaging instead of
# producing an install that can open projects but cannot export them.
require_file "$app_dir/bin/melt"

# The local CMake build links the GUI against CuteLogger from the sibling
# build directory.  The regular release bundler installs this library into
# Snapflow.app/lib, but a local package must do that explicitly or the binary
# carries a host-only RUNPATH and fails on another machine.  Keep this lookup
# relative to --gui so custom build directories continue to work.
gui_dir="$(cd "$(dirname "$gui_bin")" && pwd)"
cute_logger_bin="$gui_dir/../CuteLogger/libCuteLogger.so"
[ -f "$cute_logger_bin" ] || cute_logger_bin="$app_dir/lib/libCuteLogger.so"
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

# The MLT plugins are dlopen'ed after startup and therefore are not covered by
# the GUI executable's dependency closure. A release-style wrapper puts the
# bundle first in LD_LIBRARY_PATH, so missing plugin dependencies turn into a
# cryptic project-open failure. XML is required for project open. Some older
# local MLT builds use the libxml2.so.2 ABI while current hosts ship a newer
# SONAME; never fake that SONAME with an incompatible symlink. Allow an
# explicit SDK search path so local packaging can use a compatible copy.
runtime_lib_dirs=()
if [ -n "${SNAPFLOW_LINUX_LIB_DIRS:-}" ]; then
  IFS=: read -r -a configured_runtime_dirs <<< "$SNAPFLOW_LINUX_LIB_DIRS"
  runtime_lib_dirs+=("${configured_runtime_dirs[@]}")
fi
for ndk_root in "${ANDROID_NDK_HOME:-}" "${ANDROID_NDK_ROOT:-}" "$HOME/Android/Sdk/ndk"/*; do
  [ -d "$ndk_root/toolchains/llvm/prebuilt/linux-x86_64/lib" ] || continue
  runtime_lib_dirs+=("$ndk_root/toolchains/llvm/prebuilt/linux-x86_64/lib")
done

resolve_runtime_lib() {
  local soname="$1" dir src
  for dir in "${runtime_lib_dirs[@]}"; do
    [ -f "$dir/$soname" ] && { printf '%s\n' "$dir/$soname"; return 0; }
  done
  src=$(ldconfig -p 2>/dev/null | awk -v n="$soname" '$1 == n { print $NF; exit }')
  [ -f "$src" ] && { printf '%s\n' "$src"; return 0; }
  return 1
}

if [ ! -e "$bundle/Snapflow.app/lib/libxml2.so.2" ]; then
  xml_src="$(resolve_runtime_lib libxml2.so.2 || true)"
  if [ -z "$xml_src" ] || [ ! -f "$xml_src" ]; then
    echo "error: required libxml2.so.2 is missing; set SNAPFLOW_LINUX_LIB_DIRS or package on the Ubuntu release image" >&2
    exit 1
  fi
  install -Dm755 "$xml_src" "$bundle/Snapflow.app/lib/libxml2.so.2"
fi

# These plugins are optional. Copy compatible sonames when available, then
# remove only modules whose dependency closure is still unresolved. This
# keeps the package usable on a minimal host and makes the capability loss
# explicit rather than letting a dlopen failure obscure XML project loading.
for soname in libsox.so.3 libtheoraenc.so.1 libtheoradec.so.1 libglslang.so.15 libsndio.so.7; do
  if [ ! -e "$bundle/Snapflow.app/lib/$soname" ]; then
    src="$(resolve_runtime_lib "$soname" || true)"
    if [ -n "$src" ] && [ -f "$src" ]; then
      install -Dm755 "$src" "$bundle/Snapflow.app/lib/$soname"
    fi
  fi
done

mlt_plugin_dir="$bundle/Snapflow.app/lib/mlt-7"
if [ -f "$mlt_plugin_dir/libmltxml.so" ]; then
  xml_missing=$(LD_LIBRARY_PATH="$bundle/Snapflow.app/lib:$mlt_plugin_dir" \
    ldd "$mlt_plugin_dir/libmltxml.so" 2>/dev/null | awk '/not found/ { print $1 }')
  if [ -n "$xml_missing" ]; then
    echo "error: bundled libmltxml.so still has unresolved dependencies: $xml_missing" >&2
    exit 1
  fi
fi
for plugin in libmltsox.so libmltavformat.so; do
  plugin_path="$mlt_plugin_dir/$plugin"
  [ -f "$plugin_path" ] || continue
  missing=$(LD_LIBRARY_PATH="$bundle/Snapflow.app/lib:$mlt_plugin_dir" \
    ldd "$plugin_path" 2>/dev/null | awk '/not found/ { print $1 }')
  if [ -n "$missing" ]; then
    echo "warning: omitting optional $plugin (unresolved: $missing)" >&2
    rm -f "$plugin_path"
  fi
done

# Keep the wrapper contract used by the release bundle: the installer links
# this script, not the raw Qt binary, so the bundled media/Qt libraries are
# found without requiring LD_LIBRARY_PATH from the user's shell.
cat > "$bundle/Snapflow.app/snapflow" <<'EOF'
#!/bin/sh
set -eu
CURRENT_DIR=$(readlink -f "$0")
INSTALL_DIR=$(dirname "$CURRENT_DIR")
export LD_LIBRARY_PATH="$INSTALL_DIR/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export PATH="$INSTALL_DIR/bin${PATH:+:$PATH}"
export MELT_BIN="$INSTALL_DIR/bin/melt"
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
