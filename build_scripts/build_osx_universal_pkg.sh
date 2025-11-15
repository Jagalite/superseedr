#!/bin/bash

# SPDX-FileCopyrightText: 2025 The superseedr Contributors
# SPDX-License-Identifier: GPL-3.0-or-later

set -e # Exit immediately if a command fails
# set -x # Temporarily disabled to keep logs clean

# --- 1. SET VARIABLES FROM COMMAND LINE ARGUMENTS ---
# Usage: ./build_osx_universal_pkg.sh <VERSION> <SUFFIX> <CERT_NAME> [CARGO_FLAGS...]

INPUT_VERSION=$1       # e.g., v1.2.0
NAME_SUFFIX=$2         # e.g., "normal" or "private"
INSTALLER_CERT_NAME=$3 # e.g., "Developer ID Installer: Your Name (TEAMID)"
shift 3                # Consume the first three arguments
CARGO_FLAGS="$@"       # Use all remaining arguments as flags

# --- NEW: Derive Application cert and create entitlements ---
# Derive the Application certificate name from the Installer one
APP_CERT_NAME=$(echo "${INSTALLER_CERT_NAME}" | sed 's/Installer/Application/')
if [ "$APP_CERT_NAME" == "$INSTALLER_CERT_NAME" ]; then
    echo "::error:: Could not derive Application cert name from Installer cert name: ${INSTALLER_CERT_NAME}"
    echo "::error:: This script expects to be passed the 'Developer ID Installer' certificate."
    exit 1
fi

# Create a basic entitlements file for Hardened Runtime
ENTITLEMENTS_PATH="target/entitlements.plist"
echo "Creating entitlements file at ${ENTITLEMENTS_PATH}..."
mkdir -p target # Ensure target dir exists
cat > "${ENTITLEMENTS_PATH}" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.cs.allow-jit</key>
    <false/>
    <key>com.apple.security.cs.allow-unsigned-executable-memory</key>
    <false/>
</dict>
</plist>
EOF
# --- END NEW ---

# Fixed Application Variables
APP_NAME="superseedr"
BINARY_NAME="superseedr"
HANDLER_APP_NAME="superseedr"
PKG_IDENTIFIER="com.github.jagalite.superseedr" 
ICON_FILE_PATH="assets/app_icon.icns"
ICON_FILE_NAME="appicon.icns" 

# --- Safety Check: Icon ---
if [ ! -f "$ICON_FILE_PATH" ]; then
    echo "::error:: Icon file not found at ${ICON_FILE_PATH}"
    exit 1
fi

# Determine Version/Identifier
if [ -z "$INPUT_VERSION" ]; then
    VERSION=$(git rev-parse --short HEAD)
else
    # Strip the 'v' prefix
    VERSION=$(echo "$INPUT_VERSION" | sed 's/^v//')
fi

# Paths
TUI_BINARY_SOURCE_ARM64="target/aarch64-apple-darwin/release/${BINARY_NAME}"
TUI_BINARY_SOURCE_X86_64="target/x86_64-apple-darwin/release/${BINARY_NAME}"

HANDLER_STAGING_DIR="target/handler_staging_${NAME_SUFFIX}"
HANDLER_APP_PATH="${HANDLER_STAGING_DIR}/${HANDLER_APP_NAME}.app"
HANDLER_SCRIPT_PATH="${HANDLER_STAGING_DIR}/main.applescript"

UNIVERSAL_STAGING_DIR="target/universal_staging_${NAME_SUFFIX}"
UNIVERSAL_BINARY_PATH="${UNIVERSAL_STAGING_DIR}/${BINARY_NAME}"

if [ "$NAME_SUFFIX" == "private" ]; then
  PKG_NAME="${APP_NAME}-${VERSION}-private-universal-macos.pkg"
else
  PKG_NAME="${APP_NAME}-${VERSION}-universal-macos.pkg"
fi

PKG_OUTPUT_DIR="target/release"
UNSIGNED_PKG_OUTPUT_PATH="${PKG_OUTPUT_DIR}/${APP_NAME}-unsigned.pkg"
SIGNED_PKG_OUTPUT_PATH="${PKG_OUTPUT_DIR}/${PKG_NAME}"
PKG_STAGING_ROOT="target/pkg_staging_root_${NAME_SUFFIX}"

# Print variables for debugging
echo "--- Build Configuration (Universal PKG) ---"
echo "Version/Identifier: ${VERSION}"
echo "Build Type (Suffix): ${NAME_SUFFIX}"
echo "Installer Signer: ${INSTALLER_CERT_NAME}"
echo "Derived App Signer: ${APP_CERT_NAME}" # NEW
echo "Signed PKG Output: ${SIGNED_PKG_OUTPUT_PATH}"
echo "-------------------------------------------"

# --- 2. BUILD THE MAIN RUST TUI BINARIES (FOR BOTH ARCHS) ---
echo "Building main TUI binary for Apple Silicon (aarch64)..."
cargo build --target aarch64-apple-darwin --release $CARGO_FLAGS

echo "Building main TUI binary for Intel (x86_66)..."
cargo build --target x86_64-apple-darwin --release $CARGO_FLAGS

# --- 3. CREATE UNIVERSAL (FAT) BINARY ---
# --- Safety Check: Binaries ---
if [ ! -f "${TUI_BINARY_SOURCE_ARM64}" ] || [ ! -f "${TUI_BINARY_SOURCE_X86_64}" ]; then
    echo "::error:: One or more built binaries missing. Build failed."
    ls -l target/*/release || true
    exit 1
fi

echo "Creating universal (FAT) binary with lipo..."
rm -rf "${UNIVERSAL_STAGING_DIR}"
mkdir -p "${UNIVERSAL_STAGING_DIR}"
lipo -create \
  -output "${UNIVERSAL_BINARY_PATH}" \
  "${TUI_BINARY_SOURCE_ARM64}" \
  "${TUI_BINARY_SOURCE_X86_64}"

# --- NEW: Sign the universal binary ---
echo "Signing universal binary ${UNIVERSAL_BINARY_PATH} with Hardened Runtime..."
codesign -s "${APP_CERT_NAME}" \
  -v --deep \
  --options runtime \
  --timestamp \
  "${UNIVERSAL_BINARY_PATH}"
# --- END NEW ---

# --- 4. CREATE THE MAGNET/TORRENT HANDLER APP ---
echo "Building ${HANDLER_APP_NAME}.app programmatically..."
rm -rf "${HANDLER_STAGING_DIR}"
mkdir -p "${HANDLER_STAGING_DIR}"

# 4a. Write the AppleScript code
echo "Creating AppleScript file: ${HANDLER_SCRIPT_PATH}"
cat > "${HANDLER_SCRIPT_PATH}" << EOF
on run
    tell application "Terminal"
        activate
        do script "/usr/local/bin/${BINARY_NAME}"
    end tell
end run
on open location this_URL
    process_link(this_URL)
end open location
on open these_files
    repeat with this_file in these_files
        process_link(POSIX path of this_file)
    end repeat
end open
on process_link(the_link)
    set link_to_process to the_link as text
    if link_to_process is not "" then
        try
            set binary_path_posix to "/usr/local/bin/${BINARY_NAME}"
            set full_command to (quoted form of binary_path_posix) & " " & (quoted form of link_to_process)
            do shell script full_command & " > /dev/null 2>&1 &"
        on error errMsg
            display dialog "${HANDLER_APP_NAME} Error: " & errMsg
        end try
    end if
end process_link
EOF

# 4b. Compile the AppleScript into an Application bundle
echo "Compiling AppleScript into app bundle: ${HANDLER_APP_PATH}"
osacompile -x -o "${HANDLER_APP_PATH}" "${HANDLER_SCRIPT_PATH}"

# 4b-2. Add custom icon
echo "Adding custom icon to ${HANDLER_APP_NAME}.app..."
RESOURCES_PATH="${HANDLER_APP_PATH}/Contents/Resources"
rm -f "${RESOURCES_PATH}/droplet.icns"
rm -f "${RESOURCES_PATH}/droplets.icns"
cp "${ICON_FILE_PATH}" "${RESOURCES_PATH}/${ICON_FILE_NAME}"
echo "Custom icon added."

# 4c. Modify the Info.plist using PlistBuddy
echo "Modifying Info.plist for ${HANDLER_APP_NAME}.app..."
PLIST_PATH="${HANDLER_APP_PATH}/Contents/Info.plist"

/usr/libexec/PlistBuddy -c "Delete :CFBundleIconFile" "${PLIST_PATH}" || true
/usr/libexec/PlistBuddy -c "Add :CFBundleIconFile string ${ICON_FILE_NAME}" "${PLIST_PATH}"
/usr/libexec/PlistBuddy -c "Delete :CFBundleIdentifier" "${PLIST_PATH}" || true
/usr/libexec/PlistBuddy -c "Add :CFBundleIdentifier string ${PKG_IDENTIFIER}" "${PLIST_PATH}"
/usr/libexec/PlistBuddy -c "Delete :CFBundleSignature" "${PLIST_PATH}" || true
/usr/libexec/PlistBuddy -c "Add :CFBundleSignature string ????" "${PLIST_PATH}"

if ! /usr/libexec/PlistBuddy -c "Print :CFBundleURLTypes" "${PLIST_PATH}" &>/dev/null; then
  echo "Adding CFBundleURLTypes for magnet links..."
  /usr/libexec/PlistBuddy -c "Add :CFBundleURLTypes array" "${PLIST_PATH}"
  /usr/libexec/PlistBuddy -c "Add :CFBundleURLTypes:0 dict" "${PLIST_PATH}"
  /usr/libexec/PlistBuddy -c "Add :CFBundleURLTypes:0:CFBundleTypeRole string Viewer" "${PLIST_PATH}"
  /usr/libexec/PlistBuddy -c "Add :CFBundleURLTypes:0:CFBundleURLName string 'Magnet URI'" "${PLIST_PATH}"
  /usr/libexec/PlistBuddy -c "Add :CFBundleURLTypes:0:CFBundleURLSchemes array" "${PLIST_PATH}"
  /usr/libexec/PlistBuddy -c "Add :CFBundleURLTypes:0:CFBundleURLSchemes:0 string magnet" "${PLIST_PATH}"
fi

if ! /usr/libexec/PlistBuddy -c "Print :CFBundleDocumentTypes" "${PLIST_PATH}" &>/dev/null; then
  echo "Adding CFBundleDocumentTypes for torrent files..."
  /usr/libexec/PlistBuddy -c "Add :CFBundleDocumentTypes array" "${PLIST_PATH}"
  /usr/libexec/PlistBuddy -c "Add :CFBundleDocumentTypes:0 dict" "${PLIST_PATH}"
  /usr/libexec/PlistBuddy -c "Add :CFBundleDocumentTypes:0:CFBundleTypeRole string Viewer" "${PLIST_PATH}"
  /usr/libexec/PlistBuddy -c "Add :CFBundleDocumentTypes:0:CFBundleTypeName string 'BitTorrent File'" "${PLIST_PATH}"
  /usr/libexec/PlistBuddy -c "Add :CFBundleDocumentTypes:0:LSHandlerRank string Owner" "${PLIST_PATH}"
  /usr/libexec/PlistBuddy -c "Add :CFBundleDocumentTypes:0:LSItemContentTypes array" "${PLIST_PATH}"
  /usr/libexec/PlistBuddy -c "Add :CFBundleDocumentTypes:0:LSItemContentTypes:0 string org.bittorrent.torrent" "${PLIST_PATH}"
  /usr/libexec/PlistBuddy -c "Add :CFBundleDocumentTypes:0:CFBundleTypeExtensions array" "${PLIST_PATH}"
  /usr/libexec/PlistBuddy -c "Add :CFBundleDocumentTypes:0:CFBundleTypeExtensions:0 string torrent" "${PLIST_PATH}"
fi

# --- MODIFIED: Replace ad-hoc sign with proper Developer ID sign ---
# 4d. Sign the handler app
echo "Signing ${HANDLER_APP_NAME}.app with Developer ID and Hardened Runtime..."
codesign -s "${APP_CERT_NAME}" \
  -v --force --deep \
  --options runtime \
  --timestamp \
  --entitlements "${ENTITLEMENTS_PATH}" \
  "${HANDLER_APP_PATH}"
# --- END MODIFIED ---

# --- 5. PREPARE STAGING ROOT FOR PKG ---
echo "Staging files for PKG installer..."
rm -rf "${PKG_STAGING_ROOT}"
mkdir -p "${PKG_STAGING_ROOT}/usr/local/bin"
mkdir -p "${PKG_STAGING_ROOT}/Applications"
cp "${UNIVERSAL_BINARY_PATH}" "${PKG_STAGING_ROOT}/usr/local/bin/"
cp -R "${HANDLER_APP_PATH}" "${PKG_STAGING_ROOT}/Applications/"

# --- 6. CREATE AND SIGN THE FINAL PKG ---
echo "Creating (unsigned) PKG at ${UNSIGNED_PKG_OUTPUT_PATH}..."
mkdir -p "${PKG_OUTPUT_DIR}"
pkgbuild \
  --root "${PKG_STAGING_ROOT}" \
  --install-location "/" \
  --identifier "${PKG_IDENTIFIER}" \
  --version "${VERSION}" \
  "${UNSIGNED_PKG_OUTPUT_PATH}"

echo "Signing PKG with '${INSTALLER_CERT_NAME}'..."
productsign --sign "${INSTALLER_CERT_NAME}" \
  "${UNSIGNED_PKG_OUTPUT_PATH}" \
  "${SIGNED_PKG_OUTPUT_PATH}"
  
# --- 7. CLEAN UP ---
rm -rf "${HANDLER_STAGING_DIR}"
rm -rf "${PKG_STAGING_ROOT}"
rm -rf "${UNIVERSAL_STAGING_DIR}"
rm -f "${UNSIGNED_PKG_OUTPUT_PATH}" # Remove the unsigned original
rm -f "${ENTITLEMENTS_PATH}" # NEW: Remove entitlements file

echo ""
echo "Signed PKG creation complete at: ${SIGNED_PKG_OUTPUT_PATH}"
echo "--------------------------------------------------------"
echo "PKG_PATH=${SIGNED_PKG_OUTPUT_PATH}" # Output for GitHub Actions
echo "PKG_NAME=${PKG_NAME}" # Output the filename
