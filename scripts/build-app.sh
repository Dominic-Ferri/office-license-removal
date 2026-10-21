#!/bin/zsh
# build-app.sh — build dist/Office License Removal.app (universal, ad-hoc signed)
# and refresh dist/office-license-removal with the same universal CLI binary.
# Usage: scripts/build-app.sh
set -euo pipefail

REPO="${0:A:h:h}"
CARGO="$(command -v cargo || echo "$HOME/.cargo/bin/cargo")"
DIST="$REPO/dist"
APP="$DIST/Office License Removal.app"
MACOS="$APP/Contents/MacOS"
TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)

cd "$REPO"
for t in $TARGETS; do
  "$CARGO" build --release --workspace --target "$t"
done

# Write each output to a new file and rename it into place: overwriting a
# signed Mach-O in place can leave a stale signature cached for that inode.
universal() {  # universal <binary name> <destination>
  lipo -create -output "$2.new" "$REPO"/target/${^TARGETS}/release/"$1"
  mv -f "$2.new" "$2"
}

mkdir -p "$MACOS"
universal OfficeLicenseRemoval "$MACOS/OfficeLicenseRemoval"
universal office-license-removal "$MACOS/office-license-removal"
chmod 755 "$MACOS/OfficeLicenseRemoval" "$MACOS/office-license-removal"

VER="$("$MACOS/office-license-removal" --version | awk '{print $2}')"

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDisplayName</key><string>Office License Removal</string>
	<key>CFBundleExecutable</key><string>OfficeLicenseRemoval</string>
	<key>CFBundleIdentifier</key><string>com.dferri.office-license-removal</string>
	<key>CFBundleName</key><string>Office License Removal</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>$VER</string>
	<key>CFBundleVersion</key><string>1</string>
	<key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
	<key>LSMinimumSystemVersion</key><string>14.0</string>
	<key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

xattr -cr "$APP"
signed=0
for i in 1 2 3 4 5; do
  # The CLI is nested code inside the bundle, so it is signed first.
  if sign_err=$({ codesign --force --sign - "$MACOS/office-license-removal" \
    && codesign --force --sign - "$APP"; } 2>&1); then
    signed=1
    break
  fi
  # iCloud file provider can stamp xattrs between the clean and the sign;
  # strip and retry rather than fail the build.
  xattr -cr "$APP"
  sleep 1
done
if [[ $signed -ne 1 ]]; then
  echo "error: codesign failed 5 times; the last attempt said:" >&2
  echo "$sign_err" >&2
  exit 1
fi
codesign --verify --deep --strict "$APP"

cp "$MACOS/office-license-removal" "$DIST/office-license-removal.new"
mv -f "$DIST/office-license-removal.new" "$DIST/office-license-removal"
xattr -c "$DIST/office-license-removal"
codesign --verify --strict "$DIST/office-license-removal"

echo "OK: $APP ($VER) and $DIST/office-license-removal"
