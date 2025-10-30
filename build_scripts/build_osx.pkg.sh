#!/bin/bash
set -e # Exit immediately if a command fails

# --- 1. SET VARIABLES FROM COMMAND LINE ARGUMENTS ---
# Usage: ./build_osx_universal_pkg.sh <VERSION_OR_SHA> <NAME_SUFFIX> [CARGO_FLAGS]
# Example (Normal): ./build_osx_universal_pkg.sh v1.2.0 "normal"
# Example (Private): ./build_osx_universal_pkg.sh v1.2.0 "private" "--no-default-features"

INPUT_VERSION=$1  # e.g., v1.2.0
NAME_SUFFIX=$2    # e.g., "normal" or "private"
CARGO_FLAGS=$3    # e.g., "" or "--no-default-features"

# Fixed Application Variables
APP_NAME="superseedr"
BINARY_NAME="superseedr"
HANDLER_APP_NAME="superseedr"
PKG_IDENTIFIER="com.github.jagalite.superseedr" 
# Define the path to your .icns file
ICON_FILE_PATH="assets/app_icon.icns"
# We will overwrite the default droplet icon
ICON_FILE_NAME="droplet.icns" 

# Determine Version/Identifier
if [ -z "$INPUT_VERSION" ]; then
    VERSION=$(git rev-parse --short HEAD)
else
    VERSION="$INPUT_VERSION"
fi

# Paths
TUI_BINARY_SOURCE_ARM64="target/aarch64-apple-darwin/release/${BINARY_NAME}"
TUI_BINARY_SOURCE_X86_64="target/x86_64-apple-darwin/release/${BINARY_NAME}"

HANDLER_STAGING_DIR="target/handler_staging_${NAME_SUFFIX}"
HANDLER_APP_PATH="${HANDLER_STAGING_DIR}/${HANDLER_APP_NAME}.app"
HANDLER_SCRIPT_PATH="${HANDLER_STAGING_DIR}/main.applescript" # Temp file for the script

UNIVERSAL_STAGING_DIR="target/universal_staging_${NAME_SUFFIX}"
UNIVERSAL_BINARY_PATH="${UNIVERSAL_STAGING_DIR}/${BINARY_NAME}"

# --- MODIFIED ---
# Conditionally add the suffix to prevent double-dashes
if [ -n "$NAME_SUFFIX" ]; then
  PKG_NAME="${APP_NAME}-${VERSION}-${NAME_SUFFIX}-universal-macos.pkg"
else
  # If suffix is empty, don't add the extra dash
  PKG_NAME="${APP_NAME}-${VERSION}-universal-macos.pkg"
fi
# --- END MODIFIED ---

PKG_OUTPUT_DIR="target/release"
PKG_OUTPUT_PATH="${PKG_OUTPUT_DIR}/${PKG_NAME}"
PKG_STAGING_ROOT="target/pkg_staging_root_${NAME_SUFFIX}" # This dir will mirror the destination root (/)

# Print variables for debugging
echo "--- Build Configuration (Universal PKG) ---"
echo "Version/Identifier: ${VERSION}"
echo "Build Type (Suffix): ${NAME_SUFFIX}"
echo "Cargo Flags: ${CARGO_FLAGS}"
echo "Package Identifier: ${PKG_IDENTIFIER}"
echo "PKG Output: ${PKG_OUTPUT_PATH}"
echo "-------------------------------------------"

# --- 2. BUILD THE MAIN RUST TUI BINARIES (FOR BOTH ARCHS) ---

echo "Building main TUI binary for Apple Silicon (aarch64) with flags: ${CARGO_FLAGS}"
cargo build --target aarch64-apple-darwin --release ${CARGO_FLAGS}

echo "Building main TUI binary for Intel (x86_64) with flags: ${CARGO_FLAGS}"
cargo build --target x86_64-apple-darwin --release ${CARGO_FLAGS}


# --- 3. CREATE UNIVERSAL (FAT) BINARY ---

echo "Creating universal (FAT) binary with lipo..."
rm -rf "${UNIVERSAL_STAGING_DIR}"
mkdir -p "${UNIVERSAL_STAGING_DIR}"

lipo -create \
  -output "${UNIVERSAL_BINARY_PATH}" \
  "${TUI_BINARY_SOURCE_ARM64}" \
  "${TUI_BINARY_SOURCE_X86_64}"

echo "Universal binary info:"
lipo -info "${UNIVERSAL_BINARY_PATH}" # For verification

# --- 4. CREATE THE MAGNET/TORRENT HANDLER APP ---

echo "Building ${HANDLER_APP_NAME}.app programmatically..."
rm -rf "${HANDLER_STAGING_DIR}" # Clean previous build
mkdir -p "${HANDLER_STAGING_DIR}"

# 4a. Write the AppleScript code
echo "Creating AppleScript file: ${HANDLER_SCRIPT_PATH}"
cat > "${HANDLER_SCRIPT_PATH}" << EOF
# This handler fires when the app icon is double-clicked
on run
    # Just run the command. This is the most reliable method.
    tell application "Terminal"
        activate
        do script "${BINARY_NAME}"
    end tell
end run

# This handler fires when a URL (like a magnet link) is sent
on open location this_URL
    process_link(this_URL)
end open location

# This handler fires when a file (like a .torrent file) is double-clicked or dragged
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
if [ -f "$ICON_FILE_PATH" ]; then
    cp "${ICON_FILE_PATH}" "${HANDLER_APP_PATH}/Contents/Resources/${ICON_FILE_NAME}"
    rm -f "${HANDLER_APP_PATH}/Contents/Resources/droplets.icns"
    echo "Default droplet.icns overwritten."
else
    echo "Warning: Icon file not found at ${ICON_FILE_PATH}. Using default AppleScript icon."
fi

# 4c. Modify the Info.plist
echo "Modifying Info.plist for ${HANDLER_APP_NAME}.app..."
PLIST_PATH="${HANDLER_APP_PATH}/Contents/Info.plist"

# 4c-2. Change Bundle Identifier and Signature to look less like a script
echo "Setting CFBundleIdentifier and CFBundleSignature..."
sed -i '' "s|<key>CFBundleIdentifier</key>\s*<string>.*</string>|<key>CFBundleIdentifier</key><string>${PKG_IDENTIFIER}</string>|" "${PLIST_PATH}"
sed -i '' "s|<key>CFBundleSignature</key>\s*<string>aplt</string>|<key>CFBundleSignature</key><string>????</string>|" "${PLIST_PATH}"

# 4c-3. Magnet URI Handling
if ! grep -q "CFBundleURLTypes" "${PLIST_PATH}"; then
  sed -i '' '/<\/dict>/i \
    <key>CFBundleURLTypes</key>\
    <array>\
        <dict>\
            <key>CFBundleTypeRole</key>\
            <string>Viewer</string>\
            <key>CFBundleURLName</key>\
            <string>Magnet URI</string>\
            <key>CFBundleURLSchemes</key>\
            <array>\
                <string>magnet</string>\
            </array>\
        </dict>\
    </array>' "${PLIST_PATH}"
fi

# 4c-4. Torrent File Handling
if ! grep -q "CFBundleDocumentTypes" "${PLIST_PATH}"; then
  sed -i '' '/<\/dict>/i \
    <key>CFBundleDocumentTypes</key>\
    <array>\
        <dict>\
            <key>CFBundleTypeRole</key>\
            <string>Viewer</string>\
            <key>CFBundleTypeName</key>\
            <string>BitTorrent File</string>\
            <key>LSHandlerRank</key>\
            <string>Owner</string>\
            <key>LSItemContentTypes</key>\
            <array>\
                <string>org.bittrent.torrent</string>\
            </array>\
            <key>CFBundleTypeExtensions</key>\
            <array>\
                <string>torrent</string>\
            </Array>\
        </dict>\
    </array>' "${PLIST_PATH}"
fi

# 4d. Ad-hoc sign the handler app
echo "Signing ${HANDLER_APP_NAME}.app..."
codesign -s - --force --deep "${HANDLER_APP_PATH}"

# --- 5. PREPARE STAGING ROOT FOR PKG ---
echo "Staging files for PKG installer..."
rm -rf "${PKG_STAGING_ROOT}"

mkdir -p "${PKG_STAGING_ROOT}/usr/local/bin"
mkdir -p "${PKG_STAGING_ROOT}/Applications"

echo "Staging TUI binary to ${PKG_STAGING_ROOT}/usr/local/bin/"
cp "${UNIVERSAL_BINARY_PATH}" "${PKG_STAGING_ROOT}/usr/local/bin/"

echo "Staging Handler App to ${PKG_STAGING_ROOT}/Applications/"
cp -R "${HANDLER_APP_PATH}" "${PKG_STAGING_ROOT}/Applications/"

# --- 6. CREATE THE FINAL PKG INSTALLER ---
echo "Creating final PKG at ${PKG_OUTPUT_PATH}..."
mkdir -p "${PKG_OUTPUT_DIR}" # Ensure the final output dir exists

pkgbuild \
  --root "${PKG_STAGING_ROOT}" \
  --install-location "/" \
  --identifier "${PKG_IDENTIFIER}" \
  --version "${VERSION}" \
  "${PKG_OUTPUT_PATH}"

# --- 7. CLEAN UP ---
rm -rf "${HANDLER_STAGING_DIR}"
rm -rf "${PKG_STAGING_ROOT}"
rm -rf "${UNIVERSAL_STAGING_DIR}"

echo ""
echo "Universal PKG creation complete at: ${PKG_OUTPUT_PATH}"
echo "--------------------------------------------------------"
echo "PKG_PATH=${PKG_OUTPUT_PATH}" # Output for GitHub Actions
echo "PKG_NAME=${PKG_NAME}" # Output the filename for use in artifact name
