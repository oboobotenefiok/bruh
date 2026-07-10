#!/usr/bin/env bash
# bruh install script
# Supports: Linux x86_64, Linux arm64, macOS arm64/x86_64, and Termux, since Termux is
# basically Linux running inside Android so the same install logic mostly just works there
# too with a couple of path differences.
set -e

REPO="https://github.com/oboobotenefiok/bruh"
BIN_NAME="bruh"

# Small color helper functions, same pattern as the Rust side's is_color_enabled closures,
# just the shell equivalent. Nothing fancy, raw ANSI codes since we don't want this script
# depending on anything beyond bash and standard coreutils.
bold()  { printf "\033[1m%s\033[0m\n" "$*"; }
green() { printf "\033[32m%s\033[0m\n" "$*"; }
dim()   { printf "\033[2m%s\033[0m\n"  "$*"; }
red()   { printf "\033[31m%s\033[0m\n" "$*"; }
die()   { red "Error: $*" >&2; exit 1; }

# ── Detect platform ──────────────────────────────────────────────────────────
# We need OS and ARCH to pick the right prebuilt binary below (or fall back to building
# from source if we don't have one for this combination). Termux gets special handling
# since `uname -s` on it just reports "Linux" like any other Linux box, so we can't tell
# it apart from a normal Linux install without checking for its telltale directory.
OS=$(uname -s)
ARCH=$(uname -m)
TERMUX=false

if [ -d "/data/data/com.termux" ]; then
    TERMUX=true
    OS="Termux"
fi

echo ""
bold "  Installing bruh — persistent developer memory"
dim  "  ${REPO}"
echo ""

dim "  Detected: ${OS} / ${ARCH}"

# ── Install path ─────────────────────────────────────────────────────────────
# We prefer /usr/local/bin if we can write to it without sudo, since that's already on most
# people's PATH by default. Termux uses its own $PREFIX/bin convention instead. If neither
# works out, we fall back to ~/.local/bin and create it if needed, and warn the user below
# if that directory isn't already on their PATH.
if $TERMUX; then
    INSTALL_DIR="$PREFIX/bin"
elif [ -w "/usr/local/bin" ]; then
    INSTALL_DIR="/usr/local/bin"
else
    INSTALL_DIR="$HOME/.local/bin"
    mkdir -p "$INSTALL_DIR"
fi

# ── We try to download a prebuilt binary first ───────────────────────────────
# Building from source works everywhere but takes a couple of minutes and needs Rust
# installed, so we always try grabbing a prebuilt release binary first for the platforms we
# actually publish one for, and only fall back to compiling if that download fails or isn't
# available for this OS/ARCH combination.
RELEASE_URL="${REPO}/releases/latest/download"
BINARY_URL=""

case "${OS}/${ARCH}" in
    Linux/x86_64)         BINARY_URL="${RELEASE_URL}/bruh-linux-x86_64"  ;;
    Linux/aarch64|Linux/arm64) BINARY_URL="${RELEASE_URL}/bruh-linux-arm64" ;;
    Darwin/arm64)         BINARY_URL="${RELEASE_URL}/bruh-macos-arm64"   ;;
    Darwin/x86_64)        BINARY_URL="${RELEASE_URL}/bruh-macos-x86_64"  ;;
    Termux/*)             BINARY_URL="${RELEASE_URL}/bruh-termux-aarch64" ;;
    *)                    BINARY_URL="" ;;
esac

DOWNLOADED=false
if [ -n "$BINARY_URL" ]; then
    printf "  Downloading binary… "
    if curl -sSfL "$BINARY_URL" -o "${INSTALL_DIR}/${BIN_NAME}" 2>/dev/null; then
        chmod +x "${INSTALL_DIR}/${BIN_NAME}"
        DOWNLOADED=true
        printf "\033[32m✓\033[0m\n"
    else
        # This isn't necessarily an error, it just means there's no published release
        # binary for this platform yet (or the release page is temporarily unreachable),
        # so we quietly fall through to building from source instead of failing here.
        printf "\033[2mno release available, building from source\033[0m\n"
    fi
fi

# ── Build from source if binary unavailable ───────────────────────────────────
# This whole block only runs if the download above didn't happen, either because there's
# no prebuilt binary for this platform at all, or the download itself failed. We check for
# cargo up front and bail with a clear message rather than letting a cryptic "command not
# found" happen deeper in the script.
if ! $DOWNLOADED; then
    dim "  Building from source (requires Rust + cargo)…"
    if ! command -v cargo >/dev/null 2>&1; then
        die "cargo not found. Install Rust from https://rustup.rs and retry."
    fi

    # A fresh temp dir per run, cleaned up automatically on exit (success or failure) via
    # the trap below, so repeated installs don't leave clone artifacts scattered around.
    TMP_DIR=$(mktemp -d)
    trap 'rm -rf "$TMP_DIR"' EXIT

    printf "  Cloning repository… "
    git clone --depth=1 "$REPO" "$TMP_DIR/bruh" >/dev/null 2>&1 || die "git clone failed"
    printf "\033[32m✓\033[0m\n"

    printf "  Compiling (this takes 1–3 minutes)… "
    cd "$TMP_DIR/bruh"
    # We try the quiet build first since a clean progress line looks nicer, but if that
    # fails for some reason we rerun it verbosely so the actual compiler error is visible
    # rather than being swallowed by --quiet.
    cargo build --release --quiet 2>/dev/null || cargo build --release
    cp "target/release/bruh" "${INSTALL_DIR}/${BIN_NAME}"
    printf "\033[32m✓\033[0m\n"
    cd -
fi

# ── Verify installation ───────────────────────────────────────────────────────
# The binary being copied successfully doesn't guarantee it's actually reachable as a
# command yet, if INSTALL_DIR isn't on the user's PATH, `bruh` won't resolve. We check for
# that here and print the export line they'd need to add, rather than silently leaving them
# with a working binary they can't run without knowing the full path.
if ! command -v bruh >/dev/null 2>&1; then
    echo ""
    dim  "  Binary installed to ${INSTALL_DIR}/bruh"
    dim  "  Add it to your PATH:"
    printf "    export PATH=\"%s:\$PATH\"\n" "$INSTALL_DIR"
fi

echo ""
green "  ✓  bruh installed successfully"
echo ""

# ── Run init ──────────────────────────────────────────────────────────────────
# When this script is run the way the README recommends (curl ... | sh), stdin is the
# script's own bytes, not the user's terminal, so a plain `read` here can't reliably get
# real interactive input. [ -t 0 ] is the standard, portable way to check whether stdin is
# an actual terminal before even trying to prompt: if it's not, we skip straight to running
# init (the same thing an empty answer to the prompt would do anyway) instead of leaving the
# outcome to however that specific shell happens to handle reading from an already-consumed
# pipe.
if [ -t 0 ]; then
    printf "  Run bruh init now? [Y/n]: "
    read -r ANSWER
else
    dim "  Non-interactive install detected, running 'bruh init' with defaults."
    ANSWER="y"
fi
if [ -z "$ANSWER" ] || [ "$ANSWER" = "y" ] || [ "$ANSWER" = "Y" ]; then
    echo ""
    "${INSTALL_DIR}/${BIN_NAME}" init
else
    echo ""
    dim  "  Run 'bruh init' when ready."
    dim  "  Then start the daemon: bruh daemon &"
fi

echo ""
