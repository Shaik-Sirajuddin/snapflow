#!/usr/bin/env bash
# Snapflow installer -- downloads the latest GitHub Release bundle built by
# .github/workflows/build-linux.yml / build-macos.yml and installs it.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Shaik-Sirajuddin/snapflow/main/scripts/install.sh | bash
#
# Env overrides:
#   SNAPFLOW_VERSION   release tag to install (default: latest)
#   SNAPFLOW_INSTALL_DIR  where the bundle is unpacked (default: ~/.local/share/snapflow)
#   SNAPFLOW_BIN_DIR      where `snapflowd`/`snapflow` are symlinked (default: ~/.local/bin)

set -euo pipefail

REPO="Shaik-Sirajuddin/snapflow"
VERSION="${SNAPFLOW_VERSION:-latest}"
INSTALL_DIR="${SNAPFLOW_INSTALL_DIR:-$HOME/.local/share/snapflow}"
BIN_DIR="${SNAPFLOW_BIN_DIR:-$HOME/.local/bin}"

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

if [ "$VERSION" = "latest" ]; then
  api_url="https://api.github.com/repos/$REPO/releases/latest"
else
  api_url="https://api.github.com/repos/$REPO/releases/tags/$VERSION"
fi

info "Looking up release ($VERSION) for $platform..."
release_json="$(curl -fsSL "$api_url")" || die "failed to query $api_url -- has a release been published yet?"

# Pick the tarball asset for this platform (snapflow-linux-x86_64-*.tar.gz /
# snapflow-macos-*.tar.gz), skipping the .sha256 sidecar.
asset_url="$(printf '%s' "$release_json" \
  | grep -o "\"browser_download_url\": *\"[^\"]*snapflow-$platform[^\"]*\.tar\.gz\"" \
  | grep -v '\.sha256' \
  | head -n1 \
  | sed -E 's/.*"(https:[^"]+)"/\1/')"

[ -n "$asset_url" ] || die "no $platform tarball found in release $VERSION -- check https://github.com/$REPO/releases"

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

info "Extracting to $INSTALL_DIR..."
rm -rf "$INSTALL_DIR"
mkdir -p "$INSTALL_DIR"
tar -xzf "$archive" -C "$INSTALL_DIR" --strip-components=1

mkdir -p "$BIN_DIR"
# Renamed from snapshotd -> snapflowd (the video editor binary is itself
# named snapflow, so the daemon needed a distinct name).
ln -sf "$INSTALL_DIR/bin/snapflowd" "$BIN_DIR/snapflowd"
info "Linked snapflowd -> $BIN_DIR/snapflowd"

case "$platform" in
  linux)
    app_dir="$(find "$INSTALL_DIR" -maxdepth 1 -iname 'snapflow*' -type d | head -n1)"
    if [ -n "$app_dir" ] && [ -x "$app_dir/bin/snapflow" ]; then
      ln -sf "$app_dir/bin/snapflow" "$BIN_DIR/snapflow"
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

if ! command -v snapflowd >/dev/null 2>&1; then
  echo "note: add $BIN_DIR to your PATH to use snapflowd/snapflow from anywhere:" >&2
  echo "  export PATH=\"$BIN_DIR:\$PATH\"" >&2
fi

info "Done."
