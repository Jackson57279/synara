#!/usr/bin/env bash
# Install what the native client needs inside a Debian/Ubuntu instabox sandbox.
# Safe to run more than once.
set -euo pipefail

sudo apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
  build-essential clang cmake pkg-config \
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
  libx11-dev libx11-xcb-dev libxcb1-dev libxi-dev libxrandr-dev \
  libvulkan-dev mesa-vulkan-drivers \
  libfontconfig1-dev libfreetype-dev libssl-dev libzstd-dev libasound2-dev \
  xvfb x11-utils imagemagick xdotool >/dev/null

rustup component add clippy rustfmt >/dev/null 2>&1 || true
cd ~/synara
bun install --frozen-lockfile 2>&1 | tail -2
echo "box setup done"
