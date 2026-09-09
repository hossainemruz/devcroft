#!/usr/bin/env bash
# Install Devcroft as a desktop app, replacing the old Electron-based install.
#
#   ./scripts/install-app.sh [--debug]
#   mise run install [--debug]          # --debug installs a debug build (faster iteration)
#
# Linux:  binary   -> ~/.local/bin/devcroft
#         launcher -> ~/.local/share/applications/Devcroft.desktop (Name=Devcroft)
#         icon     -> ~/.local/share/icons/hicolor/512x512/apps/devcroft.png
#         removes the legacy com.devcroft.desktop.desktop entry and leftover
#         Electron bundle files under ~/.local/share/devcroft, while keeping
#         user data (device.json, portable/, cache/).
# macOS:  bundle   -> /Applications/Devcroft.app (~/Applications fallback
#         when /Applications is not writable), icon generated from
#         assets/icons/logo.png via sips/iconutil.
set -euo pipefail

MODE="release"
for arg in "$@"; do
  case "$arg" in
    --debug) MODE="debug" ;;
    *) echo "unknown argument: $arg" >&2; exit 1 ;;
  esac
done
# `mise run install --debug` arrives via the usage-generated env var.
if [[ "${usage_debug:-}" == "true" ]]; then
  MODE="debug"
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOGO="$ROOT/assets/icons/logo.png"
TARGET="$ROOT/target/$MODE/devcroft"

if [[ ! -f "$LOGO" ]]; then
  echo "missing logo: $LOGO" >&2
  exit 1
fi

echo "==> Building devcroft ($MODE)..."
if [[ "$MODE" == "release" ]]; then
  (cd "$ROOT" && cargo build --release)
else
  (cd "$ROOT" && cargo build)
fi

install_linux() {
  local bin="$HOME/.local/bin/devcroft"
  local desktop="$HOME/.local/share/applications/Devcroft.desktop"
  local legacy="$HOME/.local/share/applications/com.devcroft.desktop.desktop"
  local icon_dir="$HOME/.local/share/icons/hicolor/512x512/apps"
  local data_dir="${XDG_DATA_HOME:-$HOME/.local/share}/devcroft"

  mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications" "$icon_dir"
  install -m755 "$TARGET" "$bin"
  # Source is 1024x1024; toolkits downscale from the 512x512 bucket fine.
  install -m644 "$LOGO" "$icon_dir/devcroft.png"

  cat > "$desktop" <<EOF
[Desktop Entry]
Type=Application
Version=1.0
Name=Devcroft
Comment=Devcroft workspace and agent CLI
Exec=$bin app
TryExec=$bin
Icon=devcroft
Terminal=false
Categories=Development;
StartupNotify=true
StartupWMClass=devcroft
EOF

  # Replace the old Electron launcher so only one "Devcroft" remains.
  if [[ -f "$legacy" ]]; then
    rm -f "$legacy"
    echo "removed legacy launcher: $legacy"
  fi

  # Remove leftover Electron bundle files from the old install. The data
  # directory is shared with user data (device.json, portable/, cache/),
  # so only proceed when Electron markers are present and only delete
  # known Electron artifacts.
  if [[ -n "${data_dir:-}" && "$data_dir" != "$HOME" && -d "$data_dir" ]] \
    && [[ -f "$data_dir/resources/app.asar" || -f "$data_dir/libffmpeg.so" ]]; then
    rm -f "$data_dir/devcroft" \
      "$data_dir/chrome_100_percent.pak" "$data_dir/chrome_200_percent.pak" \
      "$data_dir/chrome_crashpad_handler" "$data_dir/chrome-sandbox" \
      "$data_dir/icudtl.dat" "$data_dir/libEGL.so" "$data_dir/libffmpeg.so" \
      "$data_dir/libGLESv2.so" "$data_dir/libvk_swiftshader.so" \
      "$data_dir/libvulkan.so.1" "$data_dir/LICENSE.electron.txt" \
      "$data_dir/LICENSES.chromium.html" "$data_dir/resources.pak" \
      "$data_dir/snapshot_blob.bin" "$data_dir/v8_context_snapshot.bin" \
      "$data_dir/vk_swiftshader_icd.json"
    rm -rf "$data_dir/locales" "$data_dir/resources"
    echo "removed old Electron bundle files from $data_dir (kept device.json, portable/, cache/)"
  fi

  command -v update-desktop-database >/dev/null \
    && update-desktop-database "$HOME/.local/share/applications" || true
  # Best effort only: icon lookup falls back to directory scanning when no
  # cache exists, so a failure here (e.g. an older theme dir whose cache no
  # longer validates) is not fatal and stays quiet.
  command -v gtk-update-icon-cache >/dev/null \
    && gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" >/dev/null 2>&1 || true

  echo "Installed Devcroft (linux):"
  echo "  binary:   $bin"
  echo "  launcher: $desktop"
  echo "  icon:     $icon_dir/devcroft.png"
  echo "If the launcher still shows the old entry/icon, restart your shell/launcher once."
}

install_macos() {
  local dest="/Applications/Devcroft.app"
  if [[ ! -w "/Applications" ]]; then
    dest="$HOME/Applications/Devcroft.app"
  fi
  local version
  version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$ROOT/Cargo.toml" | head -n 1)"
  version="${version:-0.1.0}"

  echo "==> Installing $dest (replaces any existing Devcroft.app)..."
  rm -rf "$dest"
  mkdir -p "$dest/Contents/MacOS" "$dest/Contents/Resources"
  install -m755 "$TARGET" "$dest/Contents/MacOS/devcroft"

  # Finder launches the bundle executable with no arguments, but bare
  # `devcroft` only prints CLI help -- the GUI needs the `app` subcommand.
  cat > "$dest/Contents/MacOS/Devcroft" <<'EOF'
#!/bin/sh
exec "$(dirname "$0")/devcroft" app "$@"
EOF
  chmod +x "$dest/Contents/MacOS/Devcroft"

  if command -v sips >/dev/null && command -v iconutil >/dev/null; then
    local workdir iconset
    workdir="$(mktemp -d)"
    iconset="$workdir/AppIcon.iconset"
    mkdir -p "$iconset"
    for size in 16 32 128 256 512; do
      sips -z "$size" "$size" "$LOGO" --out "$iconset/icon_${size}x${size}.png" >/dev/null
      sips -z "$((size * 2))" "$((size * 2))" "$LOGO" \
        --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
    done
    iconutil -c icns "$iconset" -o "$dest/Contents/Resources/AppIcon.icns"
    rm -rf "$workdir"
  else
    echo "warning: sips/iconutil not found; skipping AppIcon.icns" >&2
  fi

  cat > "$dest/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key><string>Devcroft</string>
  <key>CFBundleIdentifier</key><string>com.devcroft.desktop</string>
  <key>CFBundleName</key><string>Devcroft</string>
  <key>CFBundleDisplayName</key><string>Devcroft</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSRequiresAquaSystemAppearance</key><false/>
</dict>
</plist>
EOF
  touch "$dest"

  # CLI convenience link (best effort; ~/.local/bin may need adding to PATH).
  mkdir -p "$HOME/.local/bin"
  ln -sf "$dest/Contents/MacOS/devcroft" "$HOME/.local/bin/devcroft" || true

  echo "Installed $dest"
}

case "$(uname -s)" in
  Linux) install_linux ;;
  Darwin) install_macos ;;
  *) echo "unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac
