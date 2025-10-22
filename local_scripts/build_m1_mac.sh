# --- Define variables manually for local testing ---
APP_NAME="Superseedr" # Match your Cargo.toml bundle name
VERSION="1.0.0-test"  # Use a test version string
ARCH="aarch64"        # Or "x86_64"
TARGET_TRIPLE="aarch64-apple-darwin" # Or "x86_64-apple-darwin"

# --- Define paths ---
TARGET_DIR="target/${TARGET_TRIPLE}/release"
SOURCE_BUNDLE_DIR="${TARGET_DIR}/bundle/osx/"
DMG_NAME="${APP_NAME}-${VERSION}-${ARCH}-macos.dmg"
OUTPUT_PATH="${TARGET_DIR}/${DMG_NAME}"

cargo bundle --target aarch64-apple-darwin --release

# --- Run create-dmg ---
create-dmg \
  --volname "${APP_NAME} ${VERSION}" \
  --window-pos 200 120 \
  --window-size 800 400 \
  --icon-size 100 \
  --icon "${APP_NAME}.app" 200 190 \
  --hide-extension "${APP_NAME}.app" \
  --app-drop-link 600 185 \
  "${OUTPUT_PATH}" \
  "${SOURCE_BUNDLE_DIR}"

echo "DMG created at: ${OUTPUT_PATH}"
