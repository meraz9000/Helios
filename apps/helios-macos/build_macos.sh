#!/usr/bin/env bash
set -euo pipefail

# Build the Rust QUIC client staticlib and launch the native macOS SwiftUI + Metal viewer.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

echo "1/2 Building Rust static library (libhelios_client_core.a)..."
cargo build --release -p helios-client-core --manifest-path "${REPO_ROOT}/Cargo.toml"

echo "2/2 Building & launching HeliosViewer (SwiftUI + Metal + VideoToolbox)..."
cd "${SCRIPT_DIR}"
swift run HeliosViewer
