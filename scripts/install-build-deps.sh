#!/bin/bash
# Install the system libraries focuswm needs to link and run on Debian/Ubuntu.
#
# Shared by the Claude Code SessionStart hook and CI. The base images ship the
# runtime .so files but not the `-dev` packages, so linking fails on e.g.
# `-lxkbcommon` / `-lwayland-server` / `-lEGL` until these are installed. Xvfb is
# included so the GUI can be exercised headlessly.
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
  SUDO="sudo"
fi

# smithay (Wayland/DRM/input), Slint (fontconfig/freetype/GL) and winit's X11
# backend, plus Xvfb for headless runs.
PACKAGES=(
  pkg-config
  libxkbcommon-dev libxkbcommon-x11-dev
  libwayland-dev
  libfontconfig-dev libfreetype-dev
  libegl-dev libgl-dev libgles-dev
  libgbm-dev libdrm-dev
  libudev-dev libinput-dev libseat-dev
  libx11-dev libx11-xcb-dev
  libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev
  libxcursor-dev libxi-dev libxrandr-dev
  xvfb
)

# The main Ubuntu repos carry every package above; tolerate update errors from
# unrelated third-party PPAs that may be present and unsigned in the image.
$SUDO apt-get update -y || echo "install-build-deps: apt-get update had errors (likely unrelated PPAs); continuing."
$SUDO apt-get install -y --no-install-recommends "${PACKAGES[@]}"

echo "focuswm: system build dependencies installed."
