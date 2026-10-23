#!/bin/sh
# Build IronClaw Desktop as a macOS app and install it to ~/Applications.
#
# The bundle is what enables native notifications (macOS only delivers them
# to an app with a bundle identifier) and "Open at login". Without
# IRONCLAW_SIGN_IDENTITY the app is ad-hoc signed: it runs, but macOS refuses
# its notifications. Set it to an Apple Development identity
# (`security find-identity -v -p codesigning`) to enable them.
#
#   sh tools/ironclaw_gpui/bundle.sh             build and install
#   sh tools/ironclaw_gpui/bundle.sh --no-install build only (target/IronClaw.app)
set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
icon_src="$repo/crates/product/ironclaw_webui/frontend/public/assets/apple-touch-icon.png"
app="$here/target/IronClaw.app"
dest="$HOME/Applications/IronClaw.app"

cd "$here"
cargo build --release -p ironclaw_gpui
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/ironclaw-desktop "$app/Contents/MacOS/ironclaw-desktop"

# Icon: every size macOS asks for, scaled from the WebUI's 180 px icon.
iconset=$(mktemp -d)/AppIcon.iconset
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
  sips -z $size $size "$icon_src" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z $double $double "$icon_src" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"
rm -rf "$(dirname "$iconset")"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>IronClaw</string>
    <key>CFBundleDisplayName</key>
    <string>IronClaw</string>
    <key>CFBundleIdentifier</key>
    <string>com.ironclaw.desktop</string>
    <key>CFBundleExecutable</key>
    <string>ironclaw-desktop</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$version</string>
    <key>CFBundleVersion</key>
    <string>$version</string>
    <key>LSMinimumSystemVersion</key>
    <string>13.0</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.productivity</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF

codesign --force --sign "${IRONCLAW_SIGN_IDENTITY:--}" --timestamp=none "$app"
echo "Built $app ($version)"

[ "${1:-}" = "--no-install" ] && exit 0

# Replace, never overwrite in place: macOS kills a running binary whose
# signed file changed underneath it.
if pgrep -f "$dest/Contents/MacOS/ironclaw-desktop" >/dev/null; then
  echo "Quit IronClaw first (it is running from $dest)." >&2
  exit 1
fi
mkdir -p "$HOME/Applications"
rm -rf "$dest"
cp -R "$app" "$dest"
echo "Installed $dest"
