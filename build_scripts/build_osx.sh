#!/bin/bash
set -e # Exit immediately if a command fails

# --- 1. SET VARIABLES FROM COMMAND LINE ARGUMENTS ---
# Usage: ./build_osx.sh <TARGET_TRIPLE> <ARCH> <VERSION_OR_SHA>

TARGET_TRIPLE=$1  # e.g., aarch64-apple-darwin or x86_64-apple-darwin
ARCH=$2           # e.g., aarch64 or x86_64
INPUT_VERSION=$3  # Tag name (e.g., v1.0.0) or empty if building on a branch

# Fixed Application Variables
APP_NAME="superseedr"
BINARY_NAME="superseedr"
HANDLER_APP_NAME="superseedr_handler"

# Determine Version/Identifier
if [ -z "$INPUT_VERSION" ]; then
    # If no version is passed (building on a branch), use the short commit SHA
    VERSION=$(git rev-parse --short HEAD)
else
    VERSION="$INPUT_VERSION"
fi

# Paths
TUI_APP_SOURCE_PATH="target/${TARGET_TRIPLE}/release/bundle/osx/${APP_NAME}.app"
HANDLER_STAGING_DIR="target/handler_staging"
HANDLER_APP_PATH="${HANDLER_STAGING_DIR}/${HANDLER_APP_NAME}.app"
HANDLER_SCRIPT_PATH="${HANDLER_STAGING_DIR}/main.applescript" # Temp file for the script
DMG_NAME="${APP_NAME}-${VERSION}-${ARCH}-macos.dmg"
DMG_OUTPUT_PATH="target/${TARGET_TRIPLE}/release/${DMG_NAME}"
DMG_STAGING_DIR="target/dmg_staging"

# Print variables for debugging
echo "--- Build Configuration ---"
echo "Target: ${TARGET_TRIPLE}"
echo "Arch: ${ARCH}"
echo "Version/Identifier: ${VERSION}"
echo "DMG Output: ${DMG_OUTPUT_PATH}"
echo "---------------------------"

# --- 2. BUILD THE MAIN RUST TUI APP ---
echo "Building main TUI app (${APP_NAME}.app) using cargo bundle..."
# cargo bundle requires the target to be passed for the build step
cargo bundle --target ${TARGET_TRIPLE} --release

# --- 3. CREATE THE MAGNET/TORRENT HANDLER APP ---

echo "Building ${HANDLER_APP_NAME}.app programmatically..."
rm -rf "${HANDLER_STAGING_DIR}" # Clean previous build
mkdir -p "${HANDLER_STAGING_DIR}"

# 3a. Write the AppleScript code to handle both magnet links (open location) and torrent files (open)
echo "Creating AppleScript file: ${HANDLER_SCRIPT_PATH}"
cat > "${HANDLER_SCRIPT_PATH}" << EOF
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
            # Define the expected path to the TUI app in /Applications
            set tui_app_path_hfs to (path to applications folder as text) & "${APP_NAME}.app"
            
            # Get the POSIX path to the binary *inside* the app bundle
            set binary_path_posix to POSIX path of (tui_app_path_hfs & ":Contents:MacOS:${BINARY_NAME}") 
            
            # Build the command and run SILENTLY in the background
            set full_command to (quoted form of binary_path_posix) & " " & (quoted form of link_to_process)
            
            do shell script full_command & " > /dev/null 2>&1 &"
            
        on error errMsg
            display dialog "${HANDLER_APP_NAME} Error: " & errMsg
        end try
    end if
end process_link
EOF

# 3b. Compile the AppleScript into an Application bundle
echo "Compiling AppleScript into app bundle: ${HANDLER_APP_PATH}"
osacompile -x -o "${HANDLER_APP_PATH}" "${HANDLER_SCRIPT_PATH}"

# 3c. Modify the Info.plist to add URL handling AND File handling
echo "Modifying Info.plist for ${HANDLER_APP_NAME}.app at ${PLIST_PATH}"
PLIST_PATH="${HANDLER_APP_PATH}/Contents/Info.plist"

# --- Magnet URI Handling ---
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

# --- Torrent File Handling ---
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
            <key>CFBundleTypeIconFile</key>\
            <string></string>\
            <key>LSItemContentTypes</key>\
            <array>\
                <string>org.bittorrent.torrent</string>\
            </array>\
            <key>CFBundleTypeExtensions</key>\
            <array>\
                <string>torrent</string>\
            </array>\
        </dict>\
    </array>' "${PLIST_PATH}"
fi

# 3d. Ad-hoc sign the handler app
echo "Signing ${HANDLER_APP_NAME}.app..."
codesign -s - --force --deep "${HANDLER_APP_PATH}"

# --- 4. PREPARE AND CREATE THE FINAL DMG ---
echo "Staging apps for DMG..."
rm -rf "${DMG_STAGING_DIR}"
mkdir -p "${DMG_STAGING_DIR}"
# Copy the TUI app built by cargo-bundle
cp -R "${TUI_APP_SOURCE_PATH}" "${DMG_STAGING_DIR}/"
# Copy the Handler app we just built
cp -R "${HANDLER_APP_PATH}" "${DMG_STAGING_DIR}/"

echo "Creating final DMG at ${DMG_OUTPUT_PATH}..."
create-dmg \
  --volname "${APP_NAME} ${VERSION}" \
  --window-pos 200 120 \
  --window-size 800 400 \
  --icon-size 100 \
  --icon "${APP_NAME}.app" 175 190 \
  --hide-extension "${APP_NAME}.app" \
  --icon "${HANDLER_APP_NAME}.app" 375 190 \
  --hide-extension "${HANDLER_APP_NAME}.app" \
  --app-drop-link 600 185 \
  "${DMG_OUTPUT_PATH}" \
  "${DMG_STAGING_DIR}"

# --- 5. CLEAN UP ---
rm -rf "${HANDLER_STAGING_DIR}"
rm -rf "${DMG_STAGING_DIR}"

echo ""
echo "DMG creation complete at: ${DMG_OUTPUT_PATH}"
echo "--------------------------------------------------------"
echo "DMG_PATH=${DMG_OUTPUT_PATH}" # Output for GitHub Actions
echo "DMG_NAME=${DMG_NAME}" # Output the filename for use in artifact name
