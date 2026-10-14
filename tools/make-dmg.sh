#!/usr/bin/env bash
# Package the macOS GUI app of joplin-rs into a .dmg.
#
# Produces:
#   joplin-rs-<version>-<target>.dmg
# containing a single `joplin-rs.app` bundle (GUI only):
#   joplin-rs.app/Contents/MacOS/joplin-rs-app
#   joplin-rs.app/Contents/Resources/{joplin-rs.icns,LICENSE,README.txt}
#
# The CLI (`joplin-rs`) is NOT bundled: it ships as a plain binary in the
# tar.gz archive, exactly as on Linux.
#
# Usage: tools/make-dmg.sh [--target <rust-target>] [--bin-dir <dir>]
# Defaults: --target aarch64-apple-darwin, --bin-dir target/<target>/release
#
# Requires: macOS (hdiutil/sips/iconutil/codesign are built-in).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="aarch64-apple-darwin"
BIN_DIR=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --target)
            TARGET="$2"
            shift 2
            ;;
        --bin-dir)
            BIN_DIR="$2"
            shift 2
            ;;
        *)
            echo "unknown option: $1" >&2
            exit 2
            ;;
    esac
done

case "$TARGET" in
    *-apple-darwin) ;;
    *)
        echo "error: --target must be an *-apple-darwin target, got '$TARGET'" >&2
        exit 2
        ;;
esac

BIN_DIR="${BIN_DIR:-$REPO_ROOT/target/$TARGET/release}"
GUI_BIN="$BIN_DIR/joplin-rs-app"

if [[ ! -x "$GUI_BIN" ]]; then
    echo "error: missing binary $GUI_BIN" >&2
    echo "hint: cargo build --release --target $TARGET -p joplin-rs-app" >&2
    exit 1
fi

# Version from the binary itself (--version prints "joplin-rs-app vX.Y.Z (hash)"),
# matching the CI resolution logic.
VERSION="$("$GUI_BIN" --version | sed -E 's/.* v([0-9]+\.[0-9]+\.[0-9]+).*/\1/')"
OUT="$REPO_ROOT/joplin-rs-${VERSION}-${TARGET}.dmg"

STAGE="$(mktemp -d)"
APP="$STAGE/joplin-rs.app"
trap 'rm -rf "$STAGE"' EXIT

mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$GUI_BIN" "$APP/Contents/MacOS/joplin-rs-app"
cp "$REPO_ROOT/LICENSE" "$APP/Contents/Resources/LICENSE"
cat > "$APP/Contents/Resources/README.txt" <<EOF
joplin-rs $VERSION ($TARGET)

Drag joplin-rs.app into Applications, then launch it.

This DMG contains the GUI app only. The command-line client joplin-rs
ships as a plain binary in the tar.gz archive of the same release.

Not notarized: on first launch right-click -> Open, or allow it in
System Settings -> Privacy & Security.
EOF

# .icns: real multi-representation iconset, falling back to a single
# 1024px-representation icns generated via sips.
ICONSET="$STAGE/joplin-rs.iconset"
if [[ -x "$(command -v iconutil)" ]] && mkdir "$ICONSET" 2>/dev/null; then
    for size in 16 32 64 128 256 512 1024; do
        half=$((size / 2))
        sips -z "$size" "$size" "$REPO_ROOT/crates/app/assets/icon.png" \
            --out "$ICONSET/icon_${half}x${half}.png" >/dev/null
        sips -z "$size" "$size" "$REPO_ROOT/crates/app/assets/icon.png" \
            --out "$ICONSET/icon_${half}x${half}@2x.png" >/dev/null
    done
    iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/joplin-rs.icns"
else
    sips -s format icns "$REPO_ROOT/crates/app/assets/icon.png" \
        --out "$APP/Contents/Resources/joplin-rs.icns" >/dev/null
fi

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>joplin-rs</string>
    <key>CFBundleDisplayName</key><string>joplin-rs</string>
    <key>CFBundleIdentifier</key><string>io.github.joplin-rs.app</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleExecutable</key><string>joplin-rs-app</string>
    <key>CFBundleIconFile</key><string>joplin-rs</string>
    <key>CFBundleDevelopmentRegion</key><string>zh-CN</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
</dict>
</plist>
EOF

# Ad-hoc signature: an unsigned arm64 GUI binary is killed outright on Apple
# Silicon and Gatekeeper prompts harder otherwise. Not notarized, so the
# first-launch right-click -> Open still applies. A failed sign is not fatal.
codesign --force --sign - "$APP" >/dev/null 2>&1 ||
    echo "warning: ad-hoc codesign failed; continuing unsigned"

echo ">> creating $OUT"
rm -f "$OUT"
hdiutil create -volname "joplin-rs $VERSION" \
    -srcfolder "$STAGE" \
    -format UDZO \
    -fs HFS+ \
    -quiet \
    "$OUT"

echo ">> $OUT"
"$GUI_BIN" --version
