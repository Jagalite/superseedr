#!/bin/bash
set -e # Exit immediately if a command fails

# --- 1. DEFINE VARIABLES ---
APP_NAME="Superseedr"
BINARY_NAME="superseedr"
VERSION="1.0.0-test"
ARCH="aarch64"
TARGET_TRIPLE="aarch64-apple-darwin"

# Paths for the main TUI App
TUI_APP_SOURCE_PATH="target/${TARGET_TRIPLE}/release/bundle/osx/${APP_NAME}.app"

# Variables for the Handler App
HANDLER_APP_NAME="superseedr_handler"
HANDLER_STAGING_DIR="target/handler_staging"
HANDLER_APP_PATH="${HANDLER_STAGING_DIR}/${HANDLER_APP_NAME}.app"
HANDLER_SCRIPT_PATH="${HANDLER_STAGING_DIR}/main.applescript" # Temp file for the script

# Paths for the final DMG
DMG_NAME="${APP_NAME}-${VERSION}-${ARCH}-macos.dmg"
DMG_OUTPUT_PATH="target/${TARGET_TRIPLE}/release/${DMG_NAME}"
DMG_STAGING_DIR="target/dmg_staging" # We put BOTH apps here

# --- 2. BUILD THE MAIN RUST TUI APP ---
echo "Building main TUI app (Superseedr.app) using cargo bundle..."
# Make sure cargo bundle actually builds the superseedr binary inside
# If not, you might need a 'cargo build --bin superseedr' step first
cargo bundle --target ${TARGET_TRIPLE} --release
# We now have Superseedr.app at ${TUI_APP_SOURCE_PATH}

# --- 3. CREATE THE MAGNET/TORRENT HANDLER APP ---

echo "Building superseedr_handler.app programmatically..."
rm -rf "${HANDLER_STAGING_DIR}" # Clean previous build
mkdir -p "${HANDLER_STAGING_DIR}"

# 3a. Write the NEW AppleScript code to a temporary file
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
    # 1. Get the link (magnet or file path)
    set link_to_process to the_link as text
    
    # Only proceed if the link is not empty
    if link_to_process is not "" then
        try
            # --- FIND THE SUPERSEEDR BINARY ---
            # 2. Define the expected path to the TUI app in /Applications
            set tui_app_path_hfs to (path to applications folder as text) & "Superseedr.app"
            
            # 3. Get the POSIX path (slash-separated) to the binary *inside* the app bundle
            #    Make sure the binary name 'superseedr' here matches the actual binary name
            set binary_path_posix to POSIX path of (tui_app_path_hfs & ":Contents:MacOS:superseedr") 
            
            # --- RUN THE COMMAND ---
            # 4. Build the command to run the binary with the link/file path
            set full_command to (quoted form of binary_path_posix) & " " & (quoted form of link_to_process)
            
            # 5. Run the command SILENTLY in the background
            do shell script full_command & " > /dev/null 2>&1 &"
            
        on error errMsg
            # Optional: Show an error if it fails (e.g., Superseedr.app not found)
            display dialog "superseedr_handler Error: " & errMsg
        end try
    end if
end process_link
EOF

# 3b. Compile the AppleScript into an Application bundle
echo "Compiling AppleScript into app bundle: ${HANDLER_APP_PATH}"
osacompile -x -o "${HANDLER_APP_PATH}" "${HANDLER_SCRIPT_PATH}"

# 3c. Modify the Info.plist to add URL handling AND File handling
echo "Modifying Info.plist for ${HANDLER_APP_NAME}.app"
PLIST_PATH="${HANDLER_APP_PATH}/Contents/Info.plist"

# --- Magnet URI Handling (Existing) ---
if ! grep -q "CFBundleURLTypes" "${PLIST_PATH}"; then
  # Use standard macOS sed syntax
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
else
  echo "CFBundleURLTypes already exists in Info.plist (Skipping modification)."
fi

# --- Torrent File Handling (NEW) ---
if ! grep -q "CFBundleDocumentTypes" "${PLIST_PATH}"; then
  echo "Adding CFBundleDocumentTypes for .torrent files to Info.plist"
  # Use standard macOS sed syntax
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
else
  echo "CFBundleDocumentTypes already exists (Skipping modification for .torrent files)."
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

echo "Creating final DMG..."
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
echo "DMG created at: ${DMG_OUTPUT_PATH}"
echo "Contains: Superseedr.app (TUI) and superseedr_handler.app (Helper)"
