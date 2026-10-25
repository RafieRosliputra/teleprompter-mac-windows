#!/bin/zsh
# Build "Johor Studio Teleprompter.app" (release) into dist/ and optionally install to /Applications.
#   ./scripts/make-mac-app.sh            build only
#   ./scripts/make-mac-app.sh install    build + copy to /Applications
set -e
cd "$(dirname "$0")/.."
cargo build --release
APP="dist/Johor Studio Teleprompter.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/teleprompter-mac-windows "$APP/Contents/MacOS/teleprompter"
VER=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Johor Studio Teleprompter</string>
  <key>CFBundleDisplayName</key><string>Johor Studio Teleprompter</string>
  <key>CFBundleIdentifier</key><string>com.johorstudio.teleprompter</string>
  <key>CFBundleExecutable</key><string>teleprompter</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VER</string>
  <key>CFBundleVersion</key><string>$VER</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
codesign --force --deep -s - "$APP" 2>/dev/null || true
echo "Built: $APP"
if [[ "$1" == "install" ]]; then
  rm -rf "/Applications/Johor Studio Teleprompter.app"
  cp -R "$APP" /Applications/
  echo "Installed to /Applications"
fi
