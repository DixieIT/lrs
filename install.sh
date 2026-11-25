#!/bin/sh
# Build lrs from this checkout and put the result on PATH.
#
# Replacing a binary while it is running is the reason this is a script and not a
# `cp`: on Linux writing over a running executable fails with ETXTBSY. The new one
# is written beside it and moved into place instead, which swaps the directory
# entry and leaves the running process on the old inode until it exits. So you can
# update from inside a lrs session; the new binary is there next time.
#
#   ./install.sh                 build this checkout and install it
#   ./install.sh --pull          git pull first
#   ./install.sh --with-lele     also reinstall the `lele` server from this repo
#   ./install.sh --bin-dir DIR   somewhere other than ~/.local/bin
set -eu

BIN_DIR="${LRS_BIN_DIR:-$HOME/.local/bin}"
PULL=0
WITH_LELE=0

while [ $# -gt 0 ]; do
    case "$1" in
        --pull) PULL=1 ;;
        --with-lele) WITH_LELE=1 ;;
        --bin-dir) shift; [ $# -gt 0 ] || { echo "--bin-dir needs a path" >&2; exit 2; }; BIN_DIR="$1" ;;
        -h|--help) sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument $1 — try --help" >&2; exit 2 ;;
    esac
    shift
done

# Run from the crate, wherever the script was called from.
CRATE="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
cd "$CRATE"

if ! command -v cargo >/dev/null 2>&1; then
    echo "no cargo on PATH — install Rust from https://rustup.rs" >&2
    exit 1
fi

if [ "$PULL" -eq 1 ]; then
    echo "==> git pull"
    git -C "$CRATE/.." pull --ff-only
fi

echo "==> cargo build --release"
if ! cargo build --release; then
    # The one dependency that needs system libraries, and the one failure worth
    # naming: arboard pulls in xcb for the clipboard.
    cat >&2 <<'EOF'

The build failed. If the errors mention xcb or x11, the clipboard crate needs
system headers:

  Debian/Ubuntu  sudo apt install libxcb1-dev libxcb-render0-dev \
                                  libxcb-shape0-dev libxcb-xfixes0-dev
  Fedora         sudo dnf install libxcb-devel
  Arch           sudo pacman -S libxcb
EOF
    exit 1
fi

mkdir -p "$BIN_DIR"
TARGET="$BIN_DIR/lrs"
STAGE="$BIN_DIR/.lrs.new.$$"
trap 'rm -f "$STAGE"' EXIT INT TERM

cp target/release/lrs "$STAGE"
chmod 755 "$STAGE"
# Atomic, and it works even when the old one is running.
mv -f "$STAGE" "$TARGET"
trap - EXIT INT TERM

if [ "$WITH_LELE" -eq 1 ]; then
    if command -v uv >/dev/null 2>&1; then
        echo "==> uv tool install --reinstall (lele server)"
        (cd "$CRATE/.." && uv tool install --reinstall .)
    else
        echo "no uv on PATH — skipping the lele server" >&2
    fi
fi

echo "installed $TARGET"
"$TARGET" --help >/dev/null 2>&1 || echo "warning: the new binary would not run" >&2

# A binary somewhere the shell never looks is not installed.
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "note: $BIN_DIR is not on PATH — add it to your shell profile" >&2 ;;
esac
