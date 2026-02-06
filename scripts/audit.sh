#!/usr/bin/env bash
set -euo pipefail

if ! command -v cargo-audit &> /dev/null; then
    echo "cargo-audit not found. Install with:"
    echo "  cargo install cargo-audit"
    exit 1
fi

echo "Running cargo audit..."
cargo audit
