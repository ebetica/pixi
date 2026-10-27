#!/bin/sh
# Install the shared-cache build of pixi (https://github.com/ebetica/pixi).
#
# This is upstream pixi plus a fix that lets several users share one cache
# directory (see patches/shared-cache/README.md). Usage:
#
#   curl -fsSL https://github.com/ebetica/pixi/releases/latest/download/install.sh | sh
#
# Point everyone at a shared cache by also setting PIXI_SHARED_CACHE_DIR:
#
#   curl -fsSL https://github.com/ebetica/pixi/releases/latest/download/install.sh \
#     | PIXI_SHARED_CACHE_DIR=/tmp/pixi-cache sh
#
# That exports PIXI_CACHE_DIR and PIXI_CACHE_SHARED=1 in your shell rc, so pixi
# makes everything it creates there readable and writable by every user. Anyone
# who can log in to the host can then change the cached packages, so only use it
# for node-local caches on clusters.
#
# Environment variables:
#   PIXI_VERSION           pixi version to install, e.g. 0.83.0 for the v0.83.0.biohub release
#                          (default: latest fork release)
#   PIXI_HOME              default: ~/.pixi; the binary goes into $PIXI_HOME/bin
#   PIXI_BIN_DIR           install directory (default: $PIXI_HOME/bin)
#   PIXI_SHARED_CACHE_DIR  shared cache directory to export as PIXI_CACHE_DIR (with
#                          PIXI_CACHE_SHARED=1) in your shell rc
#   PIXI_NO_PATH_UPDATE    if set, do not edit shell rc files
#   PIXI_REPOURL           default: https://github.com/ebetica/pixi
set -eu

REPOURL="${PIXI_REPOURL:-https://github.com/ebetica/pixi}"
VERSION="${PIXI_VERSION:-latest}"
PIXI_HOME="${PIXI_HOME:-$HOME/.pixi}"
case "$PIXI_HOME" in '~' | '~'/*) PIXI_HOME="$HOME${PIXI_HOME#\~}" ;; esac
PIXI_BIN_DIR="${PIXI_BIN_DIR:-$PIXI_HOME/bin}"
SHARED_CACHE="${PIXI_SHARED_CACHE_DIR:-}"

err() { echo "error: $*" >&2; exit 1; }
warn() { echo "warn: $*" >&2; }

case "$(uname -s)/$(uname -m)" in
Linux/x86_64 | Linux/amd64) ASSET="pixi-x86_64-unknown-linux-musl.tar.gz" ;;
*) err "no shared-cache build for $(uname -s)/$(uname -m) yet (only Linux x86_64). Ask for one, or use upstream pixi." ;;
esac

if [ "$VERSION" = "latest" ]; then
    BASE="${REPOURL%/}/releases/latest/download"
else
    # Fork releases are tagged v<version>.biohub; accept 0.83.0, v0.83.0 or v0.83.0.biohub.
    V="${VERSION#v}"
    V="${V%.biohub}"
    BASE="${REPOURL%/}/releases/download/v${V}.biohub"
fi

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q "$1" -O "$2"; }
else
    err "you need curl or wget to run this script"
fi

TMP="$(mktemp -d "${TMPDIR:-/tmp}/pixi-install.XXXXXXXX")"
trap 'rm -rf "$TMP"' EXIT

echo "Downloading $BASE/$ASSET"
fetch "$BASE/$ASSET" "$TMP/$ASSET" || err "download failed: $BASE/$ASSET"

if fetch "$BASE/$ASSET.sha256" "$TMP/$ASSET.sha256" 2>/dev/null; then
    WANT="$(cut -d' ' -f1 <"$TMP/$ASSET.sha256")"
    if command -v sha256sum >/dev/null 2>&1; then
        GOT="$(sha256sum "$TMP/$ASSET" | cut -d' ' -f1)"
    else
        GOT="$(shasum -a 256 "$TMP/$ASSET" | cut -d' ' -f1)"
    fi
    [ "$WANT" = "$GOT" ] || err "checksum mismatch for $ASSET (expected $WANT, got $GOT)"
else
    warn "no checksum file published for this release; skipping verification"
fi

tar -xzf "$TMP/$ASSET" -C "$TMP"
mkdir -p "$PIXI_BIN_DIR"
# Replace atomically, so a running pixi is not overwritten in place.
cp "$TMP/pixi" "$PIXI_BIN_DIR/.pixi.new"
chmod 755 "$PIXI_BIN_DIR/.pixi.new"
mv -f "$PIXI_BIN_DIR/.pixi.new" "$PIXI_BIN_DIR/pixi"
echo "Installed $("$PIXI_BIN_DIR/pixi" --version) (shared-cache build) into $PIXI_BIN_DIR"

# --- shared cache checks -----------------------------------------------------
if [ -n "$SHARED_CACHE" ]; then
    if [ -e "$SHARED_CACHE" ] && [ ! -w "$SHARED_CACHE" ]; then
        warn "you cannot write to '$SHARED_CACHE'; its owner can fix that with: chmod -R a+rwX '$SHARED_CACHE'"
    elif [ ! -e "$SHARED_CACHE" ] && [ ! -w "$(dirname "$SHARED_CACHE")" ]; then
        warn "'$SHARED_CACHE' does not exist and you cannot create it in $(dirname "$SHARED_CACHE")"
    fi
fi

# --- shell rc ------------------------------------------------------------------
if [ -n "${PIXI_NO_PATH_UPDATE:-}" ]; then
    echo "Not updating shell rc files because PIXI_NO_PATH_UPDATE is set"
    [ -n "$SHARED_CACHE" ] && echo "Remember to set PIXI_CACHE_DIR=$SHARED_CACHE and PIXI_CACHE_SHARED=1"
    exit 0
fi

add_line() { # file line
    [ -f "$1" ] || { mkdir -p "$(dirname "$1")"; touch "$1"; }
    if ! grep -Fxq "$2" "$1"; then
        printf '\n%s\n' "$2" >>"$1"
        echo "Added to $1: $2"
    fi
}

case "$(basename "${SHELL:-}")" in
bash | zsh)
    RC="$HOME/.$(basename "$SHELL")rc"
    add_line "$RC" "export PATH=\"$PIXI_BIN_DIR:\$PATH\""
    [ -n "$SHARED_CACHE" ] && add_line "$RC" "export PIXI_CACHE_DIR=\"$SHARED_CACHE\"" && add_line "$RC" "export PIXI_CACHE_SHARED=1"
    ;;
fish)
    RC="$HOME/.config/fish/config.fish"
    add_line "$RC" "set -gx PATH \"$PIXI_BIN_DIR\" \$PATH"
    [ -n "$SHARED_CACHE" ] && add_line "$RC" "set -gx PIXI_CACHE_DIR \"$SHARED_CACHE\"" && add_line "$RC" "set -gx PIXI_CACHE_SHARED 1"
    ;;
*)
    warn "unknown shell '${SHELL:-}'; add $PIXI_BIN_DIR to PATH yourself"
    [ -n "$SHARED_CACHE" ] && warn "and export PIXI_CACHE_DIR=$SHARED_CACHE PIXI_CACHE_SHARED=1"
    RC=""
    ;;
esac
[ -n "$RC" ] && echo "Restart your shell or run: . $RC"
exit 0
