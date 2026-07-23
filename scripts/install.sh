#!/usr/bin/env bash
# Snapflow installer -- downloads the latest GitHub Release bundle built by
# .github/workflows/build-linux.yml / build-macos.yml and installs it.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Shaik-Sirajuddin/snapflow/main/scripts/install.sh | bash
#
# Env overrides:
#   SNAPFLOW_VERSION      release tag to install (default: latest)
#   SNAPFLOW_INSTALL_DIR  where the bundle is unpacked (default: ~/.local/share/snapflow)
#   SNAPFLOW_BIN_DIR      where `snapflowd`/`snapflow` are symlinked (default: ~/.local/bin)
#   SNAPFLOW_ASSET_URL    skip the GitHub API lookup and install this tarball URL directly
#                         (mainly for testing against a non-published build)
#   SNAPFLOW_SKIP_SERVICE set to 1 to skip systemd/launchd service setup

set -euo pipefail

REPO="Shaik-Sirajuddin/snapflow"
VERSION="${SNAPFLOW_VERSION:-latest}"
INSTALL_DIR="${SNAPFLOW_INSTALL_DIR:-$HOME/.local/share/snapflow}"
BIN_DIR="${SNAPFLOW_BIN_DIR:-$HOME/.local/bin}"
VERSION_FILE="$INSTALL_DIR/.snapflow-version"

die() { echo "error: $*" >&2; exit 1; }
info() { echo "==> $*"; }

command -v curl >/dev/null 2>&1 || die "curl is required"

os="$(uname -s)"
case "$os" in
  Linux)  platform="linux" ;;
  Darwin) platform="macos" ;;
  *)
    die "unsupported OS: $os. Only Linux and macOS builds are published (see .github/workflows/); on Windows, download a release asset manually from https://github.com/$REPO/releases"
    ;;
esac

arch="$(uname -m)"
case "$arch" in
  x86_64|amd64) ;;
  arm64|aarch64)
    [ "$platform" = "linux" ] && die "no Linux arm64 build is published yet; only x86_64"
    ;;
  *) die "unsupported architecture: $arch" ;;
esac

# ── Resolve the release we're targeting, then skip entirely if it's already
# installed (mirrors omni's install.sh current_version()/compare-and-skip
# pattern -- reviewed at https://github.com/Shaik-Sirajuddin/omni/blob/main/install.sh).
# snapflowd has no --version flag today, so this compares against a plain
# version-stamp file written at install time instead of querying the binary.
resolve_asset_url() {
  if [ -n "${SNAPFLOW_ASSET_URL:-}" ]; then
    printf '%s' "$SNAPFLOW_ASSET_URL"
    return
  fi

  local api_url release_json asset_url
  if [ "$VERSION" = "latest" ]; then
    api_url="https://api.github.com/repos/$REPO/releases/latest"
  else
    api_url="https://api.github.com/repos/$REPO/releases/tags/$VERSION"
  fi

  info "Looking up release ($VERSION) for $platform..." >&2
  release_json="$(curl -fsSL "$api_url")" || die "failed to query $api_url -- has a release been published yet?"

  resolved_tag="$(printf '%s' "$release_json" | grep -o '"tag_name": *"[^"]*"' | head -n1 | sed -E 's/.*"([^"]+)"$/\1/')"

  # Pick the tarball asset for this platform (snapflow-linux-x86_64-*.tar.gz /
  # snapflow-macos-*.tar.gz), skipping the .sha256 sidecar.
  asset_url="$(printf '%s' "$release_json" \
    | grep -o "\"browser_download_url\": *\"[^\"]*snapflow-$platform[^\"]*\.tar\.gz\"" \
    | grep -v '\.sha256' \
    | head -n1 \
    | sed -E 's/.*"(https:[^"]+)"/\1/')"

  [ -n "$asset_url" ] || die "no $platform tarball found in release $VERSION -- check https://github.com/$REPO/releases"
  printf '%s' "$asset_url"
}

resolved_tag=""
asset_url="$(resolve_asset_url)"
target_version="${resolved_tag:-$(basename "$asset_url")}"

if [ -f "$VERSION_FILE" ]; then
  installed_version="$(cat "$VERSION_FILE" 2>/dev/null || true)"
  if [ -n "$installed_version" ] && [ "$installed_version" = "$target_version" ] && [ -x "$BIN_DIR/snapflowd" ]; then
    info "snapflow $target_version is already installed and up to date -- nothing to do."
    info "(force a reinstall by removing $VERSION_FILE, or set SNAPFLOW_VERSION to a different tag)"
    exit 0
  fi
fi

sha_url="${asset_url}.sha256"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

archive="$tmp_dir/$(basename "$asset_url")"
info "Downloading $(basename "$asset_url")..."
# --progress-bar (not -s) so a live download percentage shows -- this is
# a multi-hundred-MB bundle (bundled video editor + all its libs), silent
# felt like a hang. The earlier metadata/API calls above stay silent
# (-s), they're small and fast.
curl -fL --progress-bar "$asset_url" -o "$archive"

if curl -fsSL "$sha_url" -o "$archive.sha256" 2>/dev/null; then
  info "Verifying checksum..."
  ( cd "$tmp_dir" && sha256sum -c "$(basename "$archive").sha256" ) \
    || die "checksum verification failed"
else
  echo "warning: no .sha256 found for this asset, skipping checksum verification" >&2
fi

# On an upgrade (not a fresh install), keep exactly one backup of the
# previous bundle before wiping it -- not a version history, just a
# bounded safety net so a botched extraction (disk full mid-write, a
# killed process) doesn't leave the user with nothing to fall back to.
# $INSTALL_DIR only ever holds static application content (binaries,
# models, shaders, licenses) -- real user/project data lives entirely
# under SNAPSHOTD_HOME (~/.snapshotd by default), a separate tree this
# script never touches, so this backup is about install resilience, not
# user-data preservation.
if [ -d "$INSTALL_DIR" ]; then
  info "Backing up previous install to $INSTALL_DIR.prev..."
  rm -rf "$INSTALL_DIR.prev"
  mv "$INSTALL_DIR" "$INSTALL_DIR.prev"
fi

info "Extracting to $INSTALL_DIR..."
mkdir -p "$INSTALL_DIR"
tar -xzf "$archive" -C "$INSTALL_DIR" --strip-components=1
echo "$target_version" > "$VERSION_FILE"

mkdir -p "$BIN_DIR"
# Renamed from snapshotd -> snapflowd (the video editor binary is itself
# named snapflow, so the daemon needed a distinct name).
ln -sf "$INSTALL_DIR/bin/snapflowd" "$BIN_DIR/snapflowd"
info "Linked snapflowd -> $BIN_DIR/snapflowd"

app_dir=""
case "$platform" in
  linux)
    # -mindepth 1 matters: without it, find also matches $INSTALL_DIR
    # itself (its own basename, .../share/snapflow, satisfies -iname
    # 'snapflow*' too) and sorts before the real Snapflow.app child dir,
    # so head -n1 silently picked the wrong (parent) directory and the
    # editor binary never got linked. Caught for real by
    # docker-tests/install-headless's headless install test.
    app_dir="$(find "$INSTALL_DIR" -mindepth 1 -maxdepth 1 -iname 'snapflow*' -type d | head -n1)"
    # Link the top-level wrapper script ($app_dir/snapflow), NOT the raw
    # binary at $app_dir/bin/snapflow -- the raw binary has no RPATH/RUNPATH
    # baked in and fails immediately with "error while loading shared
    # libraries: libCuteLogger.so: cannot open shared object file", since
    # it needs LD_LIBRARY_PATH/MLT_*/QT_PLUGIN_PATH set first. The wrapper
    # script ($app_dir/snapflow, upstream Shotcut's standard packaging
    # pattern) sets all of that up before exec'ing the real binary --
    # confirmed by its own comment: "Run this instead of trying to run
    # bin/snapflow. It runs snapflow with the correct environment."
    # Caught for real by docker-tests/install-headless trying to actually
    # launch the installed `snapflow` command, not just checking it exists.
    if [ -n "$app_dir" ] && [ -x "$app_dir/snapflow" ]; then
      ln -sf "$app_dir/snapflow" "$BIN_DIR/snapflow"
      info "Linked snapflow -> $BIN_DIR/snapflow"
    fi

    # Desktop-menu launcher (Applications grid / app search), not just a
    # CLI symlink. share/applications/*.desktop and share/icons/*.png are
    # bundled by build-linux.yml's package job.
    desktop_src="$(find "$INSTALL_DIR/share/applications" -maxdepth 1 -iname '*.desktop' 2>/dev/null | head -n1)"
    icon_src="$(find "$INSTALL_DIR/share/icons" -maxdepth 1 -iname '*.png' 2>/dev/null | head -n1)"
    if [ -n "$desktop_src" ]; then
      apps_dir="$HOME/.local/share/applications"
      icons_dir="$HOME/.local/share/icons/hicolor/128x128/apps"
      mkdir -p "$apps_dir" "$icons_dir"
      icon_name="snapflow"
      if [ -n "$icon_src" ]; then
        cp "$icon_src" "$icons_dir/$icon_name.png"
      fi
      # Exec= in the shipped .desktop file just says "snapflow" (relies on
      # PATH); point it at the real installed binary so the launcher works
      # even if $BIN_DIR isn't on PATH, and Icon= at the name we just
      # installed under the icon theme dir.
      sed -e "s|^Exec=.*|Exec=$BIN_DIR/snapflow %F|" \
          -e "s|^Icon=.*|Icon=$icon_name|" \
          "$desktop_src" > "$apps_dir/org.snapflow.Snapflow.desktop"
      info "Installed desktop launcher -> $apps_dir/org.snapflow.Snapflow.desktop"
      command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$apps_dir" >/dev/null 2>&1 || true
      command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache "$HOME/.local/share/icons/hicolor" >/dev/null 2>&1 || true
    fi
    ;;
  macos)
    dmg="$(find "$INSTALL_DIR" -maxdepth 1 -iname '*.dmg' | head -n1)"
    if [ -n "$dmg" ]; then
      info "Mounting $dmg to install Snapflow.app into /Applications..."
      mount_point="$(mktemp -d)"
      hdiutil attach "$dmg" -mountpoint "$mount_point" -nobrowse -quiet
      app="$(find "$mount_point" -maxdepth 1 -iname '*.app' | head -n1)"
      if [ -n "$app" ]; then
        rm -rf "/Applications/$(basename "$app")"
        cp -R "$app" /Applications/
        info "Installed $(basename "$app") to /Applications"
        echo "note: this build is unsigned -- right-click the app and choose Open the first time to bypass Gatekeeper." >&2
      fi
      hdiutil detach "$mount_point" -quiet
    fi
    ;;
esac

# ── Make snapflowd/snapflow usable immediately, not just after a new shell.
# A curl|bash subprocess can't mutate the calling interactive shell's PATH
# directly, so this (a) exports it for the rest of *this* script/subshells,
# and (b) persists an idempotent line into the user's shell rc file(s) for
# every future shell (same problem every curl-installer hits; rustup/nvm/
# homebrew all do some form of this).
export PATH="$BIN_DIR:$PATH"

path_line="export PATH=\"$BIN_DIR:\$PATH\"  # added by snapflow installer"
rc_files=()
case "${SHELL:-}" in
  */zsh)  rc_files+=("$HOME/.zshrc") ;;
  */bash) rc_files+=("$HOME/.bashrc") ;;
esac
# Also cover the other one if it exists, in case the user switches shells.
[ -f "$HOME/.zshrc" ] && [[ ! " ${rc_files[*]-} " == *" $HOME/.zshrc "* ]] && rc_files+=("$HOME/.zshrc")
[ -f "$HOME/.bashrc" ] && [[ ! " ${rc_files[*]-} " == *" $HOME/.bashrc "* ]] && rc_files+=("$HOME/.bashrc")
if [ "${#rc_files[@]}" -eq 0 ]; then
  rc_files+=("$HOME/.profile")
fi

case ":$PATH:" in
  *":$BIN_DIR:"*) : ;;  # already exported into this process, but rc files may still need it for future shells
esac

updated_rc=0
for rc in "${rc_files[@]}"; do
  if [ -f "$rc" ] && grep -qF "$BIN_DIR" "$rc" 2>/dev/null; then
    continue  # already persisted here
  fi
  mkdir -p "$(dirname "$rc")"
  {
    echo ""
    echo "$path_line"
  } >> "$rc"
  info "Added $BIN_DIR to PATH in $rc"
  updated_rc=1
done

# ── Daemon auto-start as a real OS service, not just an installed binary.
# snapshotd/cmd/snapshotd/main.go's own `install` subcommand is an honest
# stub (see cmdInstall) that prints what this would look like but performs
# none of it -- filled in here instead, matching how omni's own
# deployment/setup.sh (not the omni binary) owns writing the service file.
setup_linux_service() {
  local unit_dir="$HOME/.config/systemd/user"
  local unit_file="$unit_dir/snapflowd.service"
  mkdir -p "$unit_dir"
  cat > "$unit_file" <<EOF
[Unit]
Description=Snapflow agent daemon
After=network.target

[Service]
Type=simple
ExecStart=$BIN_DIR/snapflowd serve
Restart=on-failure
RestartSec=3s

[Install]
WantedBy=default.target
EOF
  info "Wrote systemd user service -> $unit_file"

  if ! command -v systemctl >/dev/null 2>&1; then
    echo "note: systemd not found -- run 'snapflowd serve' manually to start the daemon." >&2
    return
  fi
  if [ -z "${XDG_RUNTIME_DIR:-}" ] && [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
    echo "note: no active D-Bus/user session -- skipping systemd service start." >&2
    echo "  Run 'snapflowd serve' manually, or enable a lingering session and retry:" >&2
    echo "    loginctl enable-linger $(id -un)" >&2
    return
  fi
  systemctl --user daemon-reload
  if systemctl --user is-active --quiet snapflowd 2>/dev/null; then
    info "Restarting snapflowd service..."
    systemctl --user restart snapflowd
  else
    info "Enabling and starting snapflowd service..."
    systemctl --user enable --now snapflowd
  fi
}

setup_macos_service() {
  local plist_dir="$HOME/Library/LaunchAgents"
  local plist_file="$plist_dir/org.snapflow.snapflowd.plist"
  mkdir -p "$plist_dir"
  cat > "$plist_file" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>org.snapflow.snapflowd</string>
    <key>ProgramArguments</key>
    <array>
        <string>$BIN_DIR/snapflowd</string>
        <string>serve</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
</dict>
</plist>
EOF
  info "Wrote launchd agent -> $plist_file"
  if command -v launchctl >/dev/null 2>&1; then
    launchctl unload "$plist_file" >/dev/null 2>&1 || true
    launchctl load "$plist_file" 2>/dev/null \
      && info "Loaded snapflowd launchd agent" \
      || echo "note: launchctl load failed -- run 'snapflowd serve' manually." >&2
  fi
}

if [ "${SNAPFLOW_SKIP_SERVICE:-0}" != "1" ]; then
  case "$platform" in
    linux) setup_linux_service ;;
    macos) setup_macos_service ;;
  esac
fi

if [ "$updated_rc" = "1" ]; then
  echo "note: PATH updated for new shells -- run 'source ${rc_files[0]}' (or open a new terminal) to use snapflowd/snapflow in this one." >&2
fi

info "Done."
