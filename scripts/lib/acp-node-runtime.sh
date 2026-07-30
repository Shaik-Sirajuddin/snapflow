#!/usr/bin/env bash
# ACP Node/npm resolution and official local install (global-first).
#
# Source this file; do not require execute bit for library use:
#   . scripts/lib/acp-node-runtime.sh
#
# Policy (product):
#   1. Usable system node+npm(+npx) on PATH → GLOBAL
#   2. Else product bundle under $SNAPFLOW_INSTALL_DIR/runtime/node → BUNDLED
#   3. Else MISSING (caller may run acp_node_ensure)
#
# When BUNDLED wins, export SNAPFLOW_ACP_NODE_HOME and prepend bin to PATH
# for ACP children only. Does not force user shell global PATH.

# Pinned Node LTS (nodejs.org official dist). Override with SNAPFLOW_NODE_VERSION.
: "${SNAPFLOW_NODE_VERSION:=v22.14.0}"

acp_node_install_dir() {
  printf '%s' "${SNAPFLOW_INSTALL_DIR:-$HOME/.local/share/snapflow}"
}

acp_node_bundle_home() {
  if [ -n "${SNAPFLOW_ACP_NODE_HOME:-}" ] && [ -x "${SNAPFLOW_ACP_NODE_HOME}/bin/node" ]; then
    printf '%s' "$SNAPFLOW_ACP_NODE_HOME"
    return
  fi
  printf '%s' "$(acp_node_install_dir)/runtime/node"
}

# Returns 0 if prefix has executable node, npm, and npx.
acp_node_prefix_ok() {
  local p="$1"
  [ -x "$p/bin/node" ] && [ -x "$p/bin/npm" ] && [ -x "$p/bin/npx" ]
}

# System toolchain on PATH (global).
acp_node_system_ok() {
  command -v node >/dev/null 2>&1 || return 1
  command -v npm >/dev/null 2>&1 || return 1
  command -v npx >/dev/null 2>&1 || return 1
  # Reject obviously broken stubs.
  node --version >/dev/null 2>&1 || return 1
  npm --version >/dev/null 2>&1 || return 1
  return 0
}

# Print: source=global|bundled|missing
#        home=<prefix or empty>
#        node=<abs path or empty>
#        npm=<abs path or empty>
#        npx=<abs path or empty>
acp_node_resolve() {
  local home="" node_bin="" npm_bin="" npx_bin="" source="missing"

  if acp_node_system_ok; then
    source="global"
    node_bin="$(command -v node)"
    npm_bin="$(command -v npm)"
    npx_bin="$(command -v npx)"
    # Same-prefix sticky: use dirname of node for reporting home when possible.
    home="$(cd "$(dirname "$node_bin")/.." 2>/dev/null && pwd || true)"
  else
    home="$(acp_node_bundle_home)"
    if acp_node_prefix_ok "$home"; then
      source="bundled"
      node_bin="$home/bin/node"
      npm_bin="$home/bin/npm"
      npx_bin="$home/bin/npx"
    else
      home=""
    fi
  fi

  printf 'source=%s\n' "$source"
  printf 'home=%s\n' "$home"
  printf 'node=%s\n' "$node_bin"
  printf 'npm=%s\n' "$npm_bin"
  printf 'npx=%s\n' "$npx_bin"
}

acp_node_resolve_source() {
  acp_node_resolve | sed -n 's/^source=//p' | head -n1
}

# Apply env for ACP children when bundled (or force). No-op for global.
acp_node_export_for_acp() {
  local source home
  source="$(acp_node_resolve_source)"
  if [ "$source" = "bundled" ]; then
    home="$(acp_node_resolve | sed -n 's/^home=//p' | head -n1)"
    export SNAPFLOW_ACP_NODE_HOME="$home"
    export PATH="$home/bin:$PATH"
  elif [ "$source" = "global" ]; then
    # Explicitly clear force-to-bundle so children don't prefer a stale home.
    unset SNAPFLOW_ACP_NODE_HOME 2>/dev/null || true
  fi
}

acp_node_platform_arch() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Linux) os="linux" ;;
    Darwin) os="darwin" ;;
    *) echo "unsupported OS: $os" >&2; return 1 ;;
  esac
  case "$arch" in
    x86_64|amd64) arch="x64" ;;
    aarch64|arm64) arch="arm64" ;;
    *) echo "unsupported arch: $arch" >&2; return 1 ;;
  esac
  printf '%s-%s' "$os" "$arch"
}

# Download official Node into bundle home. No-op if global already OK unless FORCE=1.
acp_node_ensure() {
  local force="${1:-0}"
  local dest ver plat base url tmp archive

  if [ "$force" != "1" ] && acp_node_system_ok; then
    echo "acp-node: system node/npm present -- skipping bundled install (global-first)"
    acp_node_write_env_file
    return 0
  fi

  dest="$(acp_node_bundle_home)"
  if [ "$force" != "1" ] && acp_node_prefix_ok "$dest"; then
    echo "acp-node: bundled node already present at $dest"
    acp_node_write_env_file
    return 0
  fi

  ver="${SNAPFLOW_NODE_VERSION}"
  # Accept with or without leading v
  case "$ver" in
    v*) ;;
    *) ver="v$ver" ;;
  esac
  plat="$(acp_node_platform_arch)" || return 1
  base="node-${ver}-${plat}"
  url="${SNAPFLOW_NODE_DIST_URL:-https://nodejs.org/dist/${ver}/${base}.tar.xz}"

  command -v curl >/dev/null 2>&1 || { echo "acp-node: curl required" >&2; return 1; }
  command -v tar >/dev/null 2>&1 || { echo "acp-node: tar required" >&2; return 1; }

  tmp="$(mktemp -d)"
  archive="$tmp/${base}.tar.xz"
  echo "acp-node: downloading $url"
  if ! curl -fsSL "$url" -o "$archive"; then
    rm -rf "$tmp"
    echo "acp-node: download failed" >&2
    return 1
  fi

  mkdir -p "$(dirname "$dest")"
  rm -rf "$dest"
  if ! tar -xJf "$archive" -C "$tmp"; then
    rm -rf "$tmp"
    echo "acp-node: extract failed" >&2
    return 1
  fi
  # Official tarball expands to node-vXX-plat/
  if [ -d "$tmp/$base" ]; then
    mv "$tmp/$base" "$dest"
  else
    # Fallback: single top-level dir
    local extracted
    extracted="$(find "$tmp" -mindepth 1 -maxdepth 1 -type d | head -n1)"
    if [ -z "$extracted" ]; then
      rm -rf "$tmp"
      echo "acp-node: unexpected archive layout" >&2
      return 1
    fi
    mv "$extracted" "$dest"
  fi
  rm -rf "$tmp"

  printf '%s\n' "$ver" >"$dest/.version"
  printf '%s\n' "$url" >"$dest/.source-url"

  if ! acp_node_prefix_ok "$dest"; then
    echo "acp-node: extract incomplete at $dest" >&2
    return 1
  fi
  echo "acp-node: installed $ver -> $dest"
  acp_node_write_env_file
  return 0
}

acp_node_write_env_file() {
  local install_dir envf source home
  install_dir="$(acp_node_install_dir)"
  envf="$install_dir/env/acp-runtime.env"
  mkdir -p "$(dirname "$envf")"
  source="$(acp_node_resolve_source)"
  home="$(acp_node_resolve | sed -n 's/^home=//p' | head -n1)"
  {
    echo "# Generated by acp-node-runtime.sh — ACP processes only"
    echo "# source=$source"
    if [ "$source" = "bundled" ] && [ -n "$home" ]; then
      echo "export SNAPFLOW_ACP_NODE_HOME=\"$home\""
      echo "export PATH=\"\$SNAPFLOW_ACP_NODE_HOME/bin:\$PATH\""
    else
      echo "# using global node/npm from PATH"
      echo "unset SNAPFLOW_ACP_NODE_HOME 2>/dev/null || true"
    fi
  } >"$envf"
}

acp_node_doctor() {
  local source home node_bin npm_bin npx_bin
  eval "$(acp_node_resolve | sed 's/^/export _acp_/')"
  # portable parse
  source="$(acp_node_resolve | sed -n 's/^source=//p' | head -n1)"
  home="$(acp_node_resolve | sed -n 's/^home=//p' | head -n1)"
  node_bin="$(acp_node_resolve | sed -n 's/^node=//p' | head -n1)"
  npm_bin="$(acp_node_resolve | sed -n 's/^npm=//p' | head -n1)"
  npx_bin="$(acp_node_resolve | sed -n 's/^npx=//p' | head -n1)"

  echo "ACP Node doctor (global-first)"
  echo "  source:  $source"
  echo "  home:    ${home:-(none)}"
  echo "  node:    ${node_bin:-(missing)}"
  echo "  npm:     ${npm_bin:-(missing)}"
  echo "  npx:     ${npx_bin:-(missing)}"
  if [ -n "$node_bin" ]; then
    echo "  node -v: $($node_bin --version 2>/dev/null || echo '?')"
  fi
  if [ -n "$npm_bin" ]; then
    # npm is often a #!/usr/bin/env node script — put its bin dir first.
    local npm_dir npm_ver
    npm_dir="$(dirname "$npm_bin")"
    npm_ver="$(PATH="$npm_dir:$PATH" "$npm_bin" --version 2>/dev/null || echo '?')"
    echo "  npm -v:  $npm_ver"
  fi
  local bundle
  bundle="$(acp_node_install_dir)/runtime/node"
  if acp_node_prefix_ok "$bundle"; then
    echo "  bundle:  present ($bundle) version=$(cat "$bundle/.version" 2>/dev/null || echo '?')"
  else
    echo "  bundle:  absent ($bundle)"
  fi
  if [ "$source" = "missing" ]; then
    echo "  next:    acp_node_ensure   # or: snapflowd runtime install node"
    return 1
  fi
  return 0
}

# CLI when executed as a script
if [ "${BASH_SOURCE[0]-}" = "${0:-}" ]; then
  cmd="${1:-doctor}"
  shift || true
  case "$cmd" in
    doctor) acp_node_doctor ;;
    ensure|install)
      force=0
      [ "${1:-}" = "--force" ] && force=1
      acp_node_ensure "$force"
      ;;
    resolve) acp_node_resolve ;;
    export-env) acp_node_export_for_acp; env | grep -E 'SNAPFLOW_ACP_NODE|PATH=' | head -5 ;;
    *)
      echo "usage: $0 doctor|ensure [--force]|resolve|export-env" >&2
      exit 2
      ;;
  esac
fi
