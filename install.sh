#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# This script is part of ContextCodeCache (ccc) and is distributed under the
# MIT License (see https://github.com/colwill/ccc/LICENSE).
#
# ContextCodeCache (ccc) installer
# Auto-detects OS and architecture, downloads the matching release asset from
# https://github.com/colwill/ccc, then lets the binary install itself onto
# your PATH with `ccc install` (defaults to ~/.local/bin; no sudo).
# Release assets are single binaries named:  ccc-<os>-<arch>
#   os:   linux | macos
#   arch: x86_64 | aarch64 | armv7 | i686 | riscv64 (armv7/i686/riscv64: linux only)
#   e.g.  ccc-linux-x86_64, ccc-macos-aarch64, ccc-linux-armv7
# Windows assets (ccc-windows-<arch>.exe) also exist on the releases page for
# manual download; this installer covers Linux and macOS.
# Environment overrides:
#   CCC_VERSION      release tag to install (default: latest)
#   CCC_INSTALL_DIR  install directory (default: ~/.local/bin, via `ccc install --dir`)
#   CCC_BASE_URL     alternate asset base URL (mirrors, testing)
#   CCC_OS, CCC_ARCH skip auto-detection (values as in the asset names above)
set -euo pipefail

say()  { printf 'ccc install: %s\n' "$*"; }
fail() { printf 'ccc install: error: %s\n' "$*" >&2; exit 1; }

# --- detect platform ---
detect_os() {
  case "$(uname -s)" in
    Linux)  echo linux ;;
    Darwin) echo macos ;;
    *)      fail "unsupported OS '$(uname -s)' - this installer covers Linux and macOS; \
build from source instead (cargo build --release)" ;;
  esac
}

detect_arch() {
  case "$(uname -m)" in
    x86_64|amd64)          echo x86_64 ;;
    aarch64|arm64)         echo aarch64 ;;
    armv7l|armv6l|armhf)   echo armv7 ;;
    i386|i486|i586|i686)   echo i686 ;;
    riscv64)               echo riscv64 ;;
    *)             fail "unsupported architecture '$(uname -m)' - build from source \
instead (cargo build --release)" ;;
  esac
}

OS="${CCC_OS:-$(detect_os)}"
ARCH="${CCC_ARCH:-$(detect_arch)}"
ASSET="ccc-${OS}-${ARCH}"

# --- resolve download URL ---
REPO="https://github.com/colwill/ccc"
if [ -n "${CCC_BASE_URL:-}" ]; then
  URL="${CCC_BASE_URL%/}/${ASSET}"
elif [ -n "${CCC_VERSION:-}" ]; then
  URL="${REPO}/releases/download/${CCC_VERSION}/${ASSET}"
else
  URL="${REPO}/releases/latest/download/${ASSET}"
fi

# --- download ---
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
BIN="$TMP/ccc"

say "detected ${OS}/${ARCH} - fetching ${URL}"
if command -v curl >/dev/null 2>&1; then
  curl -fsSL --retry 2 -o "$BIN" "$URL" \
    || fail "download failed - does a release asset '${ASSET}' exist? (${URL})"
elif command -v wget >/dev/null 2>&1; then
  wget -qO "$BIN" "$URL" \
    || fail "download failed - does a release asset '${ASSET}' exist? (${URL})"
else
  fail "neither curl nor wget is available"
fi
chmod +x "$BIN"

# --- sanity check before touching the system ---
# (catches wrong-arch downloads and HTML error pages saved as 'binaries')
"$BIN" --version >/dev/null 2>&1 \
  || fail "downloaded artifact does not run on this machine (${OS}/${ARCH})"

if [ -n "${CCC_INSTALL_DIR:-}" ]; then
  "$BIN" install --force --dir "$CCC_INSTALL_DIR"
else
  "$BIN" install --force
fi

say "installed $("$BIN" --version)"
say "try: ccc serve or ccc scan ."
