#!/usr/bin/env bash
# GPUI / MarkRust Linux build dependencies (Debian/Ubuntu).
set -euo pipefail

if ! command -v apt-get >/dev/null 2>&1; then
  echo "install-linux-deps.sh supports apt-based distros only." >&2
  exit 1
fi

sudo apt-get update
sudo apt-get install -y \
  gcc g++ clang pkg-config \
  libasound2-dev \
  libfontconfig-dev libfreetype-dev \
  libwayland-dev wayland-protocols \
  libxkbcommon-dev libxkbcommon-x11-dev libx11-xcb-dev \
  libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
  libssl-dev \
  libvulkan-dev libvulkan1
