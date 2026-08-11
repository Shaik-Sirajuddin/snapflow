#!/usr/bin/env bash
# Verify and complete the runtime dependency closure of a packaged Linux GUI.
# XML is required for project open/save; optional media plugins are removed if
# their closure cannot be resolved instead of silently shipping broken dlopen
# targets.
set -euo pipefail

app_dir=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --app) [ "$#" -ge 2 ] || { echo "error: --app needs a directory" >&2; exit 2; }; app_dir="$2"; shift 2 ;;
    --help|-h) echo "Usage: scripts/check-linux-runtime-deps.sh --app DIR"; exit 0 ;;
    *) echo "error: unknown option: $1" >&2; exit 2 ;;
  esac
done
[ -n "$app_dir" ] && [ -d "$app_dir" ] || { echo "error: packaged GUI runtime not found: $app_dir" >&2; exit 1; }

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

mkdir -p "$app_dir/lib"
if [ ! -e "$app_dir/lib/libxml2.so.2" ]; then
  xml_src="$(resolve_runtime_lib libxml2.so.2 || true)"
  if [ -z "$xml_src" ] || [ ! -f "$xml_src" ]; then
    echo "error: required libxml2.so.2 is missing; set SNAPFLOW_LINUX_LIB_DIRS or use the release image" >&2
    exit 1
  fi
  install -Dm755 "$xml_src" "$app_dir/lib/libxml2.so.2"
fi

for soname in libsox.so.3 libtheoraenc.so.1 libtheoradec.so.1 libglslang.so.15 libsndio.so.7; do
  if [ ! -e "$app_dir/lib/$soname" ]; then
    src="$(resolve_runtime_lib "$soname" || true)"
    [ -n "$src" ] && [ -f "$src" ] && install -Dm755 "$src" "$app_dir/lib/$soname"
  fi
done

mlt_plugin_dir="$app_dir/lib/mlt-7"
[ -d "$mlt_plugin_dir" ] || { echo "error: MLT plugin directory missing: $mlt_plugin_dir" >&2; exit 1; }
check_required() {
  local plugin="$1" missing
  [ -f "$plugin" ] || { echo "error: required MLT module missing: $plugin" >&2; exit 1; }
  missing=$(LD_LIBRARY_PATH="$app_dir/lib:$mlt_plugin_dir" ldd "$plugin" 2>/dev/null | awk '/not found/ { print $1 }')
  [ -z "$missing" ] || { echo "error: required MLT module has unresolved dependencies: $plugin: $missing" >&2; exit 1; }
}
check_required "$mlt_plugin_dir/libmltxml.so"

for plugin in libmltsox.so libmltavformat.so; do
  plugin_path="$mlt_plugin_dir/$plugin"
  [ -f "$plugin_path" ] || continue
  missing=$(LD_LIBRARY_PATH="$app_dir/lib:$mlt_plugin_dir" ldd "$plugin_path" 2>/dev/null | awk '/not found/ { print $1 }')
  if [ -n "$missing" ]; then
    echo "warning: omitting optional $plugin (unresolved: $missing)" >&2
    rm -f "$plugin_path"
  fi
done
