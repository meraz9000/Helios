#!/usr/bin/env bash
set -euo pipefail

# Builds the full native Helios.app (SwiftUI Liquid Glass + Metal + VideoToolbox + Rust QUIC core),
# generates its Retina AppIcon (.icns), signs it ad-hoc with Xcode tools, installs it directly into
# /Applications/Helios.app, and launches it!

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
APP_NAME="Helios.app"
BUILD_APP="${SCRIPT_DIR}/.build/${APP_NAME}"
INSTALL_APP="/Applications/${APP_NAME}"

echo "==> 1/5 Building Rust QUIC client core (libhelios_client_core.a)..."
cargo build --release -p helios-client-core --manifest-path "${REPO_ROOT}/Cargo.toml"

echo "==> 2/5 Building native SwiftUI Liquid Glass + Metal + VideoToolbox binary..."
cd "${SCRIPT_DIR}"
swift build -c release

echo "==> 3/5 Rendering Retina Helios.icns app icon..."
ICONSET_DIR="${SCRIPT_DIR}/.build/Helios.iconset"
ICNS_FILE="${SCRIPT_DIR}/.build/Helios.icns"
rm -rf "${ICONSET_DIR}" "${ICNS_FILE}"
swift "${SCRIPT_DIR}/scripts/make_icon.swift" "${ICONSET_DIR}"
iconutil -c icns "${ICONSET_DIR}" -o "${ICNS_FILE}"

echo "==> 4/5 Assembling ${APP_NAME} bundle..."
rm -rf "${BUILD_APP}"
mkdir -p "${BUILD_APP}/Contents/MacOS" "${BUILD_APP}/Contents/Resources"
cp "${SCRIPT_DIR}/.build/release/HeliosViewer" "${BUILD_APP}/Contents/MacOS/Helios"
cp "${ICNS_FILE}" "${BUILD_APP}/Contents/Resources/Helios.icns"
printf "APPL????" > "${BUILD_APP}/Contents/PkgInfo"

cat > "${BUILD_APP}/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>Helios</string>
    <key>CFBundleExecutable</key>
    <string>Helios</string>
    <key>CFBundleIconFile</key>
    <string>Helios</string>
    <key>CFBundleIdentifier</key>
    <string>com.helios.viewer</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>Helios</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>0.2.0</string>
    <key>CFBundleVersion</key>
    <string>2</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.utilities</string>
    <key>LSMinimumSystemVersion</key>
    <string>14.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
    <key>NSPrincipalClass</key>
    <string>NSApplication</string>
</dict>
</plist>
PLIST

# Ad-hoc sign locally with Xcode codesign so macOS launches it cleanly with no Gatekeeper block
codesign --force --deep --sign - "${BUILD_APP}"

echo "==> 5/5 Installing into ${INSTALL_APP}..."
pkill -x Helios 2>/dev/null || true
rm -rf "${INSTALL_APP}"
cp -R "${BUILD_APP}" "${INSTALL_APP}"

echo ""
echo "Installed ${INSTALL_APP}!"
echo "Launching Helios..."
open "${INSTALL_APP}"
