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
#   SMOL_NO_MODIFY_PATH=1        do not add BIN_DIR to your shell's startup file
#   SMOL_INSECURE_SKIP_CHECKSUM=1  install without verifying the checksum (NOT recommended)
#
# Flags (each one sets the matching variable above):
#   --version VERSION   release to install, e.g. 1.29.1 or v1.29.1
#   --prefix DIR        where the bundle lives
#   --no-modify-path    do not touch your shell's startup file
#   --uninstall         remove the bundle and the `smol` symlink
#   --help              show this help
#
#   curl -sSL https://smolmachines.com/install.sh | bash -s -- --version 1.29.1
#
# The whole body runs inside main(), invoked on the LAST line, so a truncated
# `curl | bash` download never executes a partial script.
set -euo pipefail

usage() {
  sed -n '2,30p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//' || true
}

main() {
  umask 022   # never leave the install dir / binary group- or world-writable

  SMOL_REPO="${SMOL_REPO:-smol-machines/smol}"
  PREFIX="${PREFIX:-$HOME/.smol}"
  BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"

  err() { echo "error: $*" >&2; exit 1; }
  have() { command -v "$1" >/dev/null 2>&1; }

  # ── flags ─────────────────────────────────────────────────────────────────
  local UNINSTALL=0
  while [ $# -gt 0 ]; do
    case "$1" in
      --version)        [ $# -ge 2 ] || err "--version needs a value"; SMOL_VERSION="$2"; shift 2 ;;
      --version=*)      SMOL_VERSION="${1#--version=}"; shift ;;
      --prefix)         [ $# -ge 2 ] || err "--prefix needs a value"; PREFIX="$2"; shift 2 ;;
      --prefix=*)       PREFIX="${1#--prefix=}"; shift ;;
      --no-modify-path) SMOL_NO_MODIFY_PATH=1; shift ;;
      --uninstall)      UNINSTALL=1; shift ;;
      -h|--help)        usage; exit 0 ;;
      *) err "unknown option: $1 (try --help)" ;;
    esac
  done
  # A bare number is accepted for --version; releases are tagged vX.Y.Z.
  case "${SMOL_VERSION:-}" in ""|v*) ;; *) SMOL_VERSION="v$SMOL_VERSION" ;; esac

  if [ "$UNINSTALL" = 1 ]; then
    uninstall
    return
  fi

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
    # "${auth[@]+...}" guards the empty-array expansion: under `set -u`,
    # bare "${auth[@]}" on an empty array aborts on Bash 3.2 (macOS default).
    api="$(curl -fsSL "${auth[@]+"${auth[@]}"}" "https://api.github.com/repos/$SMOL_REPO/releases/latest" 2>/dev/null || true)"
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

  # Resolve the expected checksum: prefer a per-asset `<tarball>.sha256`, else
  # the combined `checksums.sha256` that every release publishes (grep this
  # tarball's line — format is `<hash>  <filename>`).
  local want="" got=""
  if curl -fsSL "$BASE/$DIST.tar.gz.sha256" -o "$TMP/$DIST.tar.gz.sha256" 2>/dev/null; then
    want="$(awk '{print $1; exit}' "$TMP/$DIST.tar.gz.sha256")"
  elif curl -fsSL "$BASE/checksums.sha256" -o "$TMP/checksums.sha256" 2>/dev/null; then
    want="$(awk -v f="$DIST.tar.gz" '$2 == f || $2 == "*" f {print $1; exit}' "$TMP/checksums.sha256")"
  fi

  if [ -n "$want" ]; then
    echo ">> verifying checksum"
    if have shasum; then got="$(shasum -a 256 "$TMP/$DIST.tar.gz" | awk '{print $1}')"
    else got="$(sha256sum "$TMP/$DIST.tar.gz" | awk '{print $1}')"; fi
    [ "$want" = "$got" ] || err "checksum mismatch (want '$want', got '$got')"
  elif [ "${SMOL_INSECURE_SKIP_CHECKSUM:-0}" = "1" ]; then
    echo ">> WARNING: checksum unavailable; SMOL_INSECURE_SKIP_CHECKSUM=1 — installing unverified"
  else
    err "checksum not found for $DIST.tar.gz (set SMOL_INSECURE_SKIP_CHECKSUM=1 to override)"
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
    *) add_to_path "$BIN_DIR" ;;
  esac

  # ── warn about OTHER smol binaries shadowing this one ─────────────────────
  # A stale copy earlier in PATH (e.g. an old `cargo install` in ~/.cargo/bin)
  # silently wins over this install: `smol` then reports an old version and
  # lacks newer subcommands — a very confusing failure. Detect and say so.
  local shadow="" dir
  local IFS_SAVE="$IFS"
  IFS=":"
  for dir in $PATH; do
    [ -z "$dir" ] && continue
    [ "$dir" = "$BIN_DIR" ] && break        # ours resolves first — all good
    if [ -x "$dir/smol" ] && [ ! -d "$dir/smol" ]; then
      shadow="$dir/smol"
      break
    fi
  done
  IFS="$IFS_SAVE"
  if [ -n "$shadow" ]; then
    echo ">> WARNING: another smol at $shadow comes BEFORE $BIN_DIR in your PATH"
    echo "   ($("$shadow" --version 2>/dev/null || echo "version unknown") — it will shadow the version you just installed)"
    echo "   remove it:  rm '$shadow'   then run:  hash -r"
  fi
  echo ">> done — try:  smol run -I alpine --net -- cat /etc/os-release"
}

# Remove what install put down: the bundle at PREFIX (only if it is a smol
# install), the symlink in BIN_DIR, and the PATH line the installer added.
# Machines, checkpoints and settings under ~/.cache and ~/.config are kept.
uninstall() {
  local removed=0
  if [ -e "$PREFIX" ]; then
    if [ -e "$PREFIX/smol-bin" ] || [ -e "$PREFIX/smol" ]; then
      rm -rf "$PREFIX"
      echo ">> removed $PREFIX"
      removed=1
    else
      echo ">> $PREFIX is not a smol install; left alone"
    fi
  fi
  if [ -L "$BIN_DIR/smol" ]; then
    rm -f "$BIN_DIR/smol"
    echo ">> removed $BIN_DIR/smol"
    removed=1
  fi
  local marker="# added by the smol installer" rc
  for rc in "${ZDOTDIR:-$HOME}/.zshrc" "$HOME/.bashrc" "$HOME/.bash_profile" "$HOME/.profile" \
            "${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/smol.fish"; do
    [ -f "$rc" ] || continue
    if grep -qF "$marker" "$rc" 2>/dev/null; then
      # Drop the marker line and the PATH line right after it.
      awk -v m="$marker" 'skip { skip = 0; next } $0 == m { skip = 1; next } { print }' "$rc" > "$rc.smol-tmp" \
        && mv "$rc.smol-tmp" "$rc" && echo ">> removed the PATH line from $rc"
    fi
  done
  [ "$removed" = 1 ] || echo ">> nothing to remove"
  echo ">> machines and settings were kept; delete ~/.cache/smolvm and ~/.config/smol to remove them too"
}

# Put BIN_DIR on PATH for future shells, as rustup and bun do: one marked line
# in the startup file of the user's shell, added once. The running shell is a
# parent of `curl | bash` and cannot be changed from here, so say how to use
# smol right now too. SMOL_NO_MODIFY_PATH=1 keeps every file untouched.
add_to_path() {
  local dir="$1" marker="# added by the smol installer" display rc line
  display="$dir"
  case "$dir" in "$HOME"/*) display="\$HOME${dir#"$HOME"}" ;; esac
  local now=">> to use smol in this shell now:  export PATH=\"$display:\$PATH\""
  if [ "${SMOL_NO_MODIFY_PATH:-}" = 1 ]; then
    echo ">> add $dir to your PATH:  export PATH=\"$display:\$PATH\""
    return
  fi
  local files=()
  case "$(basename "${SHELL:-}")" in
    zsh) files=("${ZDOTDIR:-$HOME}/.zshrc") ;;
    bash)
      files=("$HOME/.bashrc")
      # macOS Terminal starts bash as a login shell, which reads .bash_profile.
      [ "$(uname -s)" = Darwin ] && files+=("$HOME/.bash_profile")
      ;;
    fish) files=("${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/smol.fish") ;;
    *) files=("$HOME/.profile") ;;
  esac
  local added=()
  for rc in "${files[@]}"; do
    case "$rc" in
      *.fish) line="fish_add_path \"$display\"" ;;
      *) line="export PATH=\"$display:\$PATH\"" ;;
    esac
    if [ -f "$rc" ] && grep -qF "$marker" "$rc" 2>/dev/null; then
      continue
    fi
    if mkdir -p "$(dirname "$rc")" 2>/dev/null && printf '\n%s\n%s\n' "$marker" "$line" >>"$rc" 2>/dev/null; then
      added+=("$rc")
    else
      echo ">> could not update $rc; add $dir to your PATH yourself"
    fi
  done
  if [ "${#added[@]}" -gt 0 ]; then
    echo ">> added $dir to your PATH in ${added[*]} (opt out: SMOL_NO_MODIFY_PATH=1)"
  fi
  echo "$now"
}

main "$@"
