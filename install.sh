#!/bin/bash

set -e

REPO="Blankeos/cftun"
BINARY_NAME="cftun"

if command -v cargo &> /dev/null; then
    echo "📦 Installing ${BINARY_NAME} via cargo..."
    cargo install "${BINARY_NAME}"
    echo "✓ ${BINARY_NAME} installed successfully via cargo"
    echo ""
    echo "Run: ${BINARY_NAME}"
    exit 0
fi

echo "⬇️ Installing ${BINARY_NAME} via cargo-dist installer..."

curl --proto "=https" --tlsv1.2 -LsSf "https://github.com/${REPO}/releases/latest/download/${BINARY_NAME}-installer.sh" | sh

echo ""
echo "Run: ${BINARY_NAME}"
