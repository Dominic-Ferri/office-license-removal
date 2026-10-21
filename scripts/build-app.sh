#!/bin/zsh
# build-app.sh — build dist/Office License Removal.app (universal, ad-hoc signed)
# and refresh dist/office-license-removal with the same universal CLI binary.
# Usage: scripts/build-app.sh
set -euo pipefail

REPO="${0:A:h:h}"
CARGO="$(command -v cargo || echo "$HOME/.cargo/bin/cargo")"
DIST="$REPO/dist"
APP="$DIST/Office License Removal.app"
TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)

cd "$REPO"
for t in $TARGETS; do
  "$CARGO" build --release --workspace --target "$t"
done

# iCloud Drive stamps Finder attributes onto a bundle in a synced folder (such
# as ~/Documents) within a fraction of a second, and codesign refuses a bundle
# that carries them. So the bundle is assembled, signed and checked in a new
# staging folder outside the repo, then moved into dist/. New files in a new
# folder also avoid a stale signature cached for an overwritten inode.
STAGE="$(mktemp -d "${TMPDIR:-/tmp}/olr-build.XXXXXX")"
STAGED="$STAGE/Office License Removal.app"
MACOS="$STAGED/Contents/MacOS"

universal() {  # universal <binary name> <destination>
  lipo -create -output "$2" "$REPO"/target/${^TARGETS}/release/"$1"
}

mkdir -p "$MACOS"
universal OfficeLicenseRemoval "$MACOS/OfficeLicenseRemoval"
universal office-license-removal "$MACOS/office-license-removal"
chmod 755 "$MACOS/OfficeLicenseRemoval" "$MACOS/office-license-removal"

VER="$("$MACOS/office-license-removal" --version | awk '{print $2}')"

cat > "$STAGED/Contents/Info.plist" <<EOF
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

# The CLI is nested code inside the bundle, so it is signed first.
codesign --force --sign - "$MACOS/office-license-removal"
codesign --force --sign - "$STAGED"
codesign --verify --deep --strict "$STAGED"

cp "$MACOS/office-license-removal" "$STAGE/office-license-removal"

# Replace the previous build: it moves into the staging folder.
mkdir -p "$DIST"
if [[ -e "$APP" ]]; then
  mv "$APP" "$STAGE/previous.app"
fi
mv "$STAGED" "$APP"
# Not --strict: in a synced folder, iCloud may already have stamped the bundle.
codesign --verify --deep "$APP"

mv -f "$STAGE/office-license-removal" "$DIST/office-license-removal"
xattr -c "$DIST/office-license-removal"
codesign --verify --strict "$DIST/office-license-removal"

echo "OK: $APP ($VER) and $DIST/office-license-removal"
