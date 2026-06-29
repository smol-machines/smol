#!/usr/bin/env bash
# install.sh — install the self-contained `smol` CLI. No smolvm required.
#
#   curl -sSL https://raw.githubusercontent.com/smol-machines/smol/main/scripts/install.sh | bash
#
# Downloads the smol-<version>-<platform> release bundle (which carries its own
# libkrun runtime + guest agent), verifies its checksum, extracts it to a prefix,
# and symlinks `smol` onto your PATH. The bundle is relocatable — its wrapper
# finds lib/ + agent-rootfs/ relative to itself — so installing is just extract +
# symlink; there is no separate runtime to place.
#
# Env (all optional):
#   SMOL_VERSION                 release tag to install, e.g. v1.0.1 (default: latest)
#   SMOL_REPO                    GitHub repo to install from (default: smol-machines/smol)
#   PREFIX                       where the bundle lives (default: ~/.smol)
#   BIN_DIR                      where the `smol` symlink goes (default: ~/.local/bin)
#   GITHUB_TOKEN                 optional, raises the GitHub API rate limit for "latest"
#   SMOL_INSECURE_SKIP_CHECKSUM=1  install without verifying the checksum (NOT recommended)
#
# The whole body runs inside main(), invoked on the LAST line, so a truncated
# `curl | bash` download never executes a partial script.
set -euo pipefail

main() {
  umask 022   # never leave the install dir / binary group- or world-writable

  SMOL_REPO="${SMOL_REPO:-smol-machines/smol}"
  PREFIX="${PREFIX:-$HOME/.smol}"
  BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"

  err() { echo "error: $*" >&2; exit 1; }
  have() { command -v "$1" >/dev/null 2>&1; }

  # ── refuse to rm -rf a dangerous / non-smol PREFIX ────────────────────────
  case "$PREFIX" in
    ""|"/"|"$HOME") err "refusing to install into PREFIX='$PREFIX' (would rm -rf it)" ;;
  esac
  if [ -e "$PREFIX" ] && [ ! -e "$PREFIX/smol-bin" ]; then
    err "$PREFIX already exists and is not a smol install — remove it yourself or set PREFIX"
  fi

  # ── platform ──────────────────────────────────────────────────────────────
  local os arch
  case "$(uname -s)" in
    Darwin) os=darwin ;;
    Linux)  os=linux ;;
    *) err "unsupported OS: $(uname -s)" ;;
  esac
  case "$(uname -m)" in
    arm64|aarch64) arch=arm64 ;;
    x86_64|amd64)  arch=x86_64 ;;
    *) err "unsupported arch: $(uname -m)" ;;
  esac
  if [ "$os" = darwin ] && [ "$arch" != arm64 ]; then
    err "no smol build for $os-$arch (darwin is Apple Silicon only)"
  fi
  local PLATFORM="${os}-${arch}"

  # ── version ───────────────────────────────────────────────────────────────
  local VERSION="${SMOL_VERSION:-}"
  if [ -z "$VERSION" ]; then
    echo ">> resolving latest release of $SMOL_REPO"
    local auth=()
    [ -n "${GITHUB_TOKEN:-}" ] && auth=(-H "Authorization: Bearer $GITHUB_TOKEN")
    local api
    api="$(curl -fsSL "${auth[@]}" "https://api.github.com/repos/$SMOL_REPO/releases/latest" 2>/dev/null || true)"
    VERSION="$(printf '%s' "$api" | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')"
    if [ -z "$VERSION" ]; then
      if printf '%s' "$api" | grep -q "rate limit"; then
        err "GitHub API rate limit hit — set SMOL_VERSION=vX.Y.Z (or GITHUB_TOKEN) and retry"
      fi
      err "could not resolve latest release of $SMOL_REPO (set SMOL_VERSION=vX.Y.Z)"
    fi
  fi
  local VER="${VERSION#v}"
  local DIST="smol-${VER}-${PLATFORM}"
  # SMOL_BASE_URL overrides the download root (corporate mirror / air-gapped /
  # local testing); default is the GitHub release for this version.
  local BASE="${SMOL_BASE_URL:-https://github.com/$SMOL_REPO/releases/download/$VERSION}"

  echo ">> installing smol $VERSION ($PLATFORM)"

  # ── download + verify ─────────────────────────────────────────────────────
  # NOT local: the EXIT trap fires in global scope after main() returns, so it
  # must see TMP there. ${TMP:-} keeps the trap safe under `set -u` if we exit
  # before this assignment.
  TMP="$(mktemp -d)"
  trap 'rm -rf "${TMP:-}"' EXIT
  echo ">> downloading $DIST.tar.gz"
  curl -fSL --progress-bar "$BASE/$DIST.tar.gz" -o "$TMP/$DIST.tar.gz" || err "download failed"

  if curl -fsSL "$BASE/$DIST.tar.gz.sha256" -o "$TMP/$DIST.tar.gz.sha256" 2>/dev/null; then
    echo ">> verifying checksum"
    ( cd "$TMP"
      local want got
      want="$(awk '{print $1}' "$DIST.tar.gz.sha256")"
      if have shasum; then got="$(shasum -a 256 "$DIST.tar.gz" | awk '{print $1}')"
      else got="$(sha256sum "$DIST.tar.gz" | awk '{print $1}')"; fi
      [ -n "$want" ] && [ "$want" = "$got" ] || err "checksum mismatch (want '$want', got '$got')"
    )
  elif [ "${SMOL_INSECURE_SKIP_CHECKSUM:-0}" = "1" ]; then
    echo ">> WARNING: checksum file missing; SMOL_INSECURE_SKIP_CHECKSUM=1 — installing unverified"
  else
    err "checksum file not found for $DIST.tar.gz (set SMOL_INSECURE_SKIP_CHECKSUM=1 to override)"
  fi

  # ── extract on the prefix's filesystem, then mv into place atomically ─────
  echo ">> installing to $PREFIX"
  tar -xzf "$TMP/$DIST.tar.gz" -C "$TMP"
  [ -f "$TMP/$DIST/smol-bin" ] || err "bundle is missing smol-bin — download may be corrupt"
  local STAGE="${PREFIX}.new.$$"
  rm -rf "$STAGE" "$PREFIX"
  mv "$TMP/$DIST" "$STAGE" 2>/dev/null || { mkdir -p "$STAGE" && cp -a "$TMP/$DIST/." "$STAGE/"; }
  mv "$STAGE" "$PREFIX"
  chmod +x "$PREFIX/smol" "$PREFIX/smol-bin"

  # macOS: a downloaded tarball gets the com.apple.quarantine xattr, which blocks
  # the ad-hoc-signed binary + dylibs from loading. Clear it across the bundle.
  if [ "$os" = darwin ] && have xattr; then
    xattr -dr com.apple.quarantine "$PREFIX" 2>/dev/null || true
  fi

  # ── symlink onto PATH ─────────────────────────────────────────────────────
  mkdir -p "$BIN_DIR"
  [ -d "$BIN_DIR/smol" ] && err "$BIN_DIR/smol is a directory — remove it and re-run"
  rm -f "$BIN_DIR/smol"
  ln -s "$PREFIX/smol" "$BIN_DIR/smol"
  echo ">> linked $BIN_DIR/smol -> $PREFIX/smol"

  # ── verify it actually runs, then PATH hint ───────────────────────────────
  if "$PREFIX/smol" --version >/dev/null 2>&1; then
    echo ">> installed: $("$PREFIX/smol" --version 2>/dev/null || echo "smol $VERSION")"
  else
    echo ">> WARNING: installed smol $VERSION, but '$PREFIX/smol --version' did not run cleanly"
    echo "   on macOS this is usually Gatekeeper — try: xattr -dr com.apple.quarantine '$PREFIX'"
  fi
  case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo ">> add $BIN_DIR to your PATH:  export PATH=\"$BIN_DIR:\$PATH\"" ;;
  esac
  echo ">> done — try:  smol run -I alpine --net -- cat /etc/os-release"
}

main "$@"
