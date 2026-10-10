#!/usr/bin/env bash
# Install native build prerequisites for Devcroft CI on Ubuntu 22.04 (x86_64).
# Determined from the locked dependency graph (GPUI Wayland/X11/xkb, onig via
# cc/pkg-config, Zig via mise) and Zed's script/linux reference; validated by
# a fresh-runner clean build in CI, not by copying a Tauri/Electron list.
#
# Usage: ./scripts/ci/install-linux-deps.sh
set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "install-linux-deps.sh only runs on Linux (got $(uname -s))" >&2
  exit 1
fi

APT="$(command -v apt-get || true)"
if [[ -z "$APT" ]]; then
  echo "apt-get not found; extend this script for the runner distro" >&2
  exit 1
fi

if [[ "$(id -u)" -eq 0 ]]; then
  MAY_SUDO=""
else
  MAY_SUDO="$(command -v sudo || true)"
fi

PACKAGES=(
  build-essential
  clang
  lld
  cmake
  pkg-config
  # GPUI Linux windowing (X11 + Wayland + xkb)
  libx11-dev
  libx11-xcb-dev
  libxcb1-dev
  libxcb-render0-dev
  libxcb-shape0-dev
  libxcb-xfixes0-dev
  libxkbcommon-dev
  libxkbcommon-x11-dev
  libwayland-dev
  # GPU/windowing runtime headers for wgpu builds
  libvulkan-dev
  # Text/font stack (cosmic-text/fontdb may use fontconfig)
  libfontconfig-dev
  # CI utilities used by fixtures and scripts
  git
  curl
  jq
  python3
)

echo "==> Updating apt..."
$MAY_SUDO "$APT" update

echo "==> Installing Devcroft Linux prerequisites..."
$MAY_SUDO "$APT" install -y "${PACKAGES[@]}"

echo "==> Versions:"
cc --version | head -n 1
clang --version | head -n 1
pkg-config --version
cmake --version | head -n 1
git --version
python3 --version
