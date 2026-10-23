#!/bin/sh
# Build IronClaw Desktop as a macOS app and install it to ~/Applications.
#
# The bundle is what enables native notifications (macOS only delivers them
# to an app with a bundle identifier) and "Open at login". The app is signed
# with IRONCLAW_SIGN_IDENTITY, else the first valid "Apple Development"
# identity in the keychain, else ad-hoc. An ad-hoc signed app runs, but macOS
# refuses its notifications.
#
#   sh tools/ironclaw_gpui/bundle.sh             build and install
#   sh tools/ironclaw_gpui/bundle.sh --no-install build only (target/IronClaw.app)
set -eu

here=$(cd "$(dirname "$0")" && pwd)
icon_src="$here/app/assets/icon.png"
app="$here/target/IronClaw.app"
dest="$HOME/Applications/IronClaw.app"

cd "$here"
cargo build --release -p ironclaw_gpui
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/ironclaw-desktop "$app/Contents/MacOS/ironclaw-desktop"

# Icon: every size macOS asks for, scaled from the 1024 px app icon.
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

identity="${IRONCLAW_SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null | awk '/Apple Development/{print $2; exit}')}"
[ -n "$identity" ] || echo "warning: no signing identity, ad-hoc signing (notifications will not work)" >&2
codesign --force --sign "${identity:--}" --timestamp=none "$app"
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
