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
#     | PIXI_SHARED_CACHE_DIR=/shared/pixi-cache sh
#
# Environment variables:
#   PIXI_VERSION           pixi version to install, e.g. 0.83.0 (default: latest fork release)
#   PIXI_HOME              default: ~/.pixi; the binary goes into $PIXI_HOME/bin
#   PIXI_BIN_DIR           install directory (default: $PIXI_HOME/bin)
#   PIXI_SHARED_CACHE_DIR  shared cache directory to export as PIXI_CACHE_DIR in your shell rc
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
    V="${VERSION#v}"
    BASE="${REPOURL%/}/releases/download/v${V%-shared-cache}-shared-cache"
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
    if [ ! -d "$SHARED_CACHE" ]; then
        warn "shared cache directory '$SHARED_CACHE' does not exist. An admin should create it once:"
        echo "      sudo mkdir -p '$SHARED_CACHE' && sudo chgrp <shared-group> '$SHARED_CACHE' && sudo chmod 2775 '$SHARED_CACHE'" >&2
    elif [ ! -w "$SHARED_CACHE" ]; then
        warn "you cannot write to '$SHARED_CACHE'; ask an admin to add you to its group ($(ls -ld "$SHARED_CACHE" | awk '{print $4}'))"
    else
        case "$(ls -ld "$SHARED_CACHE" | cut -c1-10)" in
        d?????[sS]???) ;;
        *) warn "'$SHARED_CACHE' is missing the setgid bit, so new files may not get the shared group (fix: chmod 2775 '$SHARED_CACHE')" ;;
        esac
    fi
    case "$(umask)" in
    *[2367]?) warn "your umask ($(umask)) removes group write, so other users cannot update cache entries you create. Add 'umask 002' to your shell rc." ;;
    esac
fi

# --- shell rc ------------------------------------------------------------------
if [ -n "${PIXI_NO_PATH_UPDATE:-}" ]; then
    echo "Not updating shell rc files because PIXI_NO_PATH_UPDATE is set"
    [ -n "$SHARED_CACHE" ] && echo "Remember to set PIXI_CACHE_DIR=$SHARED_CACHE"
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
    [ -n "$SHARED_CACHE" ] && add_line "$RC" "export PIXI_CACHE_DIR=\"$SHARED_CACHE\""
    ;;
fish)
    RC="$HOME/.config/fish/config.fish"
    add_line "$RC" "set -gx PATH \"$PIXI_BIN_DIR\" \$PATH"
    [ -n "$SHARED_CACHE" ] && add_line "$RC" "set -gx PIXI_CACHE_DIR \"$SHARED_CACHE\""
    ;;
*)
    warn "unknown shell '${SHELL:-}'; add $PIXI_BIN_DIR to PATH yourself"
    [ -n "$SHARED_CACHE" ] && warn "and export PIXI_CACHE_DIR=$SHARED_CACHE"
    RC=""
    ;;
esac
[ -n "$RC" ] && echo "Restart your shell or run: . $RC"
exit 0
