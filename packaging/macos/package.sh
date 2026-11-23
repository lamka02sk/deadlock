#!/usr/bin/env bash
# Builds the release binary and packs it as Deadlock.app inside a .dmg in target/package/.
# Runs on macOS only: needs iconutil, codesign and hdiutil (all part of the system).
# The package is for the architecture of the machine that builds it.
set -euo pipefail
cd "$(dirname "$0")/../.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/deadlock/Cargo.toml | head -n1)
arch=$(uname -m)
out=target/package
app="$out/dmg/Deadlock.app"

cargo build --release --locked -p deadlock
rm -rf "$out/dmg" "$out"/*.dmg
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

cp target/release/deadlock "$app/Contents/MacOS/deadlock"
sed "s/@VERSION@/$version/" packaging/macos/Info.plist > "$app/Contents/Info.plist"

# The .icns from the PNGs the Linux packages install; "@2x" is the size doubled.
iconset="$out/deadlock.iconset"
rm -rf "$iconset"
mkdir -p "$iconset"
for s in 16 32 128 256; do
    cp "packaging/linux/icons/deadlock-$s.png" "$iconset/icon_${s}x${s}.png"
    cp "packaging/linux/icons/deadlock-$((s * 2)).png" "$iconset/icon_${s}x${s}@2x.png"
done
cp packaging/linux/icons/deadlock-512.png "$iconset/icon_512x512.png"
iconutil -c icns -o "$app/Contents/Resources/deadlock.icns" "$iconset"
rm -rf "$iconset"

# shortcut: ad-hoc signature, so Gatekeeper asks the user to allow the app by hand;
# sign with a Developer ID and notarize once there is an Apple Developer account.
codesign --force --sign - "$app"

ln -s /Applications "$out/dmg/Applications"
hdiutil create -volname Deadlock -srcfolder "$out/dmg" -ov -format UDZO \
    "$out/Deadlock-$version-$arch.dmg"
rm -rf "$out/dmg"
ls -l "$out"
