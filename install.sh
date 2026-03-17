#!/usr/bin/env sh
# install.sh — download and install carnelia-collab from GitHub Releases
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Agate-DB/Carnelia-Collab/master/install.sh | sh
#   VERSION=v0.1.1 sh install.sh

set -e

REPO="Agate-DB/Carnelia-Collab"
BIN="carnelia-collab"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

# --- resolve version ---
if [ -z "$VERSION" ]; then
    VERSION=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name"' | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')
fi

if [ -z "$VERSION" ]; then
    echo "error: could not determine latest version" >&2
    exit 1
fi

# --- detect platform ---
OS=$(uname -s)
ARCH=$(uname -m)

case "${OS}" in
    Linux*)
        case "${ARCH}" in
            x86_64)  TARGET="x86_64-unknown-linux-musl" ;;
            aarch64) TARGET="aarch64-unknown-linux-musl" ;;
            arm64)   TARGET="aarch64-unknown-linux-musl" ;;
            *)       echo "error: unsupported architecture: ${ARCH}" >&2; exit 1 ;;
        esac
        ARCHIVE="${BIN}-${TARGET}.tar.gz"
        ;;
    *)
        echo "error: unsupported OS: ${OS}. Use the Windows installer (install.ps1) on Windows." >&2
        exit 1
        ;;
esac

URL="https://github.com/${REPO}/releases/download/${VERSION}/${ARCHIVE}"

echo "Installing ${BIN} ${VERSION} for ${TARGET}..."
echo "Downloading ${URL}"

# --- download and extract ---
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

curl -fsSL "$URL" -o "${TMPDIR}/${ARCHIVE}"
tar -xzf "${TMPDIR}/${ARCHIVE}" -C "${TMPDIR}"

# --- install ---
mkdir -p "$INSTALL_DIR"
mv "${TMPDIR}/${BIN}" "${INSTALL_DIR}/${BIN}"
chmod +x "${INSTALL_DIR}/${BIN}"

echo ""
echo "${BIN} ${VERSION} installed to ${INSTALL_DIR}/${BIN}"

# Warn if INSTALL_DIR is not in PATH
case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        echo ""
        echo "WARNING: ${INSTALL_DIR} is not in your PATH."
        echo "Add the following to your shell profile (~/.bashrc, ~/.zshrc, etc.):"
        echo ""
        echo "  export PATH=\"\$PATH:${INSTALL_DIR}\""
        ;;
esac
