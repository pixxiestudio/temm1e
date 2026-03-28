#!/bin/sh
# TEMM1E Installer — one-line install for macOS and Linux
#
# Usage:
#   curl -sSfL https://raw.githubusercontent.com/temm1e-labs/temm1e/main/install.sh | sh
#
# This script:
#   1. Detects your OS and architecture
#   2. Downloads the latest pre-built binary from GitHub Releases
#   3. Installs it to ~/.local/bin/temm1e (or /usr/local/bin with --global)
#   4. Verifies the checksum
#
# No Rust toolchain required.

set -e

REPO="temm1e-labs/temm1e"
# Default to ~/bin if it exists and is in PATH, otherwise ~/.local/bin
if [ -d "${HOME}/bin" ] && echo "$PATH" | grep -q "${HOME}/bin"; then
    INSTALL_DIR="${HOME}/bin"
else
    INSTALL_DIR="${HOME}/.local/bin"
fi
BINARY_NAME="temm1e"
GLOBAL=false

# Parse args
for arg in "$@"; do
    case "$arg" in
        --global) GLOBAL=true; INSTALL_DIR="/usr/local/bin" ;;
        --help|-h)
            echo "Usage: install.sh [--global]"
            echo ""
            echo "  --global    Install to /usr/local/bin (requires sudo)"
            echo "  (default)   Install to ~/.local/bin"
            exit 0
            ;;
    esac
done

# Colors (only if terminal supports them)
# No colors — keeps output clean when piped through sh
info()  { printf "> %s\n" "$1"; }
warn()  { printf "! %s\n" "$1"; }
error() { printf "x %s\n" "$1"; exit 1; }

# Detect OS
OS="$(uname -s)"
case "$OS" in
    Linux*)  PLATFORM="linux" ;;
    Darwin*) PLATFORM="macos" ;;
    *)       error "Unsupported OS: $OS. TEMM1E supports Linux and macOS." ;;
esac

# Detect architecture
ARCH="$(uname -m)"
case "$ARCH" in
    x86_64|amd64)  ARCH_TAG="x86_64" ;;
    aarch64|arm64) ARCH_TAG="aarch64" ;;
    *)             error "Unsupported architecture: $ARCH. TEMM1E supports x86_64 and aarch64." ;;
esac

# On Linux, prefer the desktop binary (includes computer use / desktop control).
# Fall back to the server binary if desktop variant isn't available.
if [ "$PLATFORM" = "linux" ]; then
    ARTIFACT="${BINARY_NAME}-${ARCH_TAG}-${PLATFORM}-desktop"
    FALLBACK_ARTIFACT="${BINARY_NAME}-${ARCH_TAG}-${PLATFORM}"
else
    ARTIFACT="${BINARY_NAME}-${ARCH_TAG}-${PLATFORM}"
    FALLBACK_ARTIFACT=""
fi

info "Detected: ${PLATFORM} ${ARCH_TAG}"

# Get latest release tag
info "Finding latest release..."
LATEST_TAG=$(curl -sSfL "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null \
    | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')

if [ -z "$LATEST_TAG" ]; then
    error "Could not find latest release. Check https://github.com/${REPO}/releases"
fi

info "Latest version: ${LATEST_TAG}"

# Download binary
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${LATEST_TAG}/${ARTIFACT}"
CHECKSUM_URL="https://github.com/${REPO}/releases/download/${LATEST_TAG}/checksums-sha256.txt"

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

info "Downloading ${ARTIFACT}..."
if ! curl -sSfL -o "${TMPDIR}/${ARTIFACT}" "$DOWNLOAD_URL"; then
    if [ -n "$FALLBACK_ARTIFACT" ]; then
        info "Desktop binary not available, trying server binary..."
        ARTIFACT="$FALLBACK_ARTIFACT"
        DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${LATEST_TAG}/${ARTIFACT}"
        if ! curl -sSfL -o "${TMPDIR}/${ARTIFACT}" "$DOWNLOAD_URL"; then
            error "Download failed. Binary may not exist for your platform (${ARTIFACT})."
        fi
        info "Note: Server binary installed (no desktop control). For desktop control, build from source:"
        info "  cargo install --git https://github.com/${REPO}"
    else
        error "Download failed. Binary may not exist for your platform (${ARTIFACT})."
    fi
fi

# Verify checksum
info "Verifying checksum..."
if curl -sSfL -o "${TMPDIR}/checksums.txt" "$CHECKSUM_URL" 2>/dev/null; then
    EXPECTED=$(grep "${ARTIFACT}" "${TMPDIR}/checksums.txt" | awk '{print $1}')
    if [ -n "$EXPECTED" ]; then
        if command -v sha256sum >/dev/null 2>&1; then
            ACTUAL=$(sha256sum "${TMPDIR}/${ARTIFACT}" | awk '{print $1}')
        elif command -v shasum >/dev/null 2>&1; then
            ACTUAL=$(shasum -a 256 "${TMPDIR}/${ARTIFACT}" | awk '{print $1}')
        else
            warn "No sha256sum or shasum found — skipping checksum verification"
            ACTUAL="$EXPECTED"
        fi

        if [ "$EXPECTED" != "$ACTUAL" ]; then
            error "Checksum mismatch! Expected: ${EXPECTED}, Got: ${ACTUAL}"
        fi
        info "Checksum verified"
    else
        warn "No checksum found for ${ARTIFACT} — skipping verification"
    fi
else
    warn "Could not download checksums — skipping verification"
fi

# Install
mkdir -p "$INSTALL_DIR"
if [ "$GLOBAL" = true ]; then
    info "Installing to ${INSTALL_DIR} (may require sudo)..."
    sudo cp "${TMPDIR}/${ARTIFACT}" "${INSTALL_DIR}/${BINARY_NAME}"
    sudo chmod +x "${INSTALL_DIR}/${BINARY_NAME}"
else
    cp "${TMPDIR}/${ARTIFACT}" "${INSTALL_DIR}/${BINARY_NAME}"
    chmod +x "${INSTALL_DIR}/${BINARY_NAME}"
fi

# Verify installation
if "${INSTALL_DIR}/${BINARY_NAME}" --version >/dev/null 2>&1; then
    VERSION=$("${INSTALL_DIR}/${BINARY_NAME}" --version 2>&1 || echo "unknown")
    info "Installed: ${VERSION}"
else
    info "Installed to ${INSTALL_DIR}/${BINARY_NAME}"
fi

# Check PATH
case ":$PATH:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        warn "${INSTALL_DIR} is not in your PATH."
        echo ""
        echo "  Add this to your shell profile (~/.bashrc, ~/.zshrc, etc.):"
        echo ""
        echo "    export PATH=\"${INSTALL_DIR}:\$PATH\""
        echo ""
        ;;
esac

echo ""
printf "TEMM1E installed!\n"
echo ""
echo "  Quick start:"
echo "    temm1e setup          # Interactive setup wizard"
echo "    temm1e auth login     # Authenticate with ChatGPT (optional)"
echo "    temm1e start          # Start the bot"
echo ""
echo "  Full guide: https://github.com/${REPO}#quick-start"
echo ""
