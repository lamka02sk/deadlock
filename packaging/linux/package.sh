#!/usr/bin/env bash
# Builds the release binary and packs it as a .deb and an AppImage in target/package/.
# Usage: package.sh [deb|appimage|all] (default all).
# Needs dpkg-deb and dpkg-shlibdeps (dpkg-dev); the AppImage also needs curl and
# desktop-file-validate (desktop-file-utils), and
# downloads appimagetool into target/package-tools/ the first time. No root needed.
set -euo pipefail
cd "$(dirname "$0")/../.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/deadlock/Cargo.toml | head -n1)
out=target/package
tools=target/package-tools
appimagetool_version=1.9.0
what=${1:-all}

cargo build --release --locked -p deadlock
mkdir -p "$out" "$tools"

# The files both packages install, under the root $1.
stage() {
    install -Dm755 target/release/deadlock "$1/usr/bin/deadlock"
    install -Dm644 packaging/linux/deadlock.desktop "$1/usr/share/applications/deadlock.desktop"
    for s in 16 24 32 48 64 128 256 512; do
        install -Dm644 "packaging/linux/icons/deadlock-$s.png" \
            "$1/usr/share/icons/hicolor/${s}x${s}/apps/deadlock.png"
    done
    install -Dm644 crates/deadlock/assets/icons/app-logo.svg \
        "$1/usr/share/icons/hicolor/scalable/apps/deadlock.svg"
}

# .deb: linked libraries from dpkg-shlibdeps, plus the ones loaded at run time through
# dlopen: the Vulkan loader and a driver for drawing, and libwayland-client. Fonts
# are bundled, so fontconfig is not needed.
if [ "$what" != appimage ]; then
deb="$out/deb"
rm -rf "$deb" "$out"/*.deb
stage "$deb"
mkdir -p "$deb/DEBIAN" "$out/shlibs/debian"
printf 'Source: deadlock\n\nPackage: deadlock\nArchitecture: any\n' > "$out/shlibs/debian/control"
linked=$(cd "$out/shlibs" && dpkg-shlibdeps -O "$PWD/../deb/usr/bin/deadlock" | sed 's/^shlibs:Depends=//')
cat > "$deb/DEBIAN/control" <<CONTROL
Package: deadlock
Version: $version
Architecture: amd64
Maintainer: Deadlock developers <noreply@localhost>
Section: database
Priority: optional
Depends: $linked, libvulkan1, mesa-vulkan-drivers | vulkan-icd, libwayland-client0
Description: Database IDE for MySQL, MariaDB, Oracle and Redis
 Native query editor, table data editor, explain plans, sessions and locks,
 server configuration and schema diagrams.
CONTROL
dpkg-deb --root-owner-group -Zxz --build "$deb" "$out/deadlock_${version}_amd64.deb"
rm -rf "$out/shlibs" "$deb"
fi

# AppImage: the same files, with the libraries every desktop has left to the host.
if [ "$what" != deb ]; then
appdir="$out/Deadlock.AppDir"
rm -rf "$appdir" "$out"/*.AppImage
stage "$appdir"
ln -s usr/bin/deadlock "$appdir/AppRun"
cp packaging/linux/deadlock.desktop "$appdir/"
cp packaging/linux/icons/deadlock-256.png "$appdir/deadlock.png"
tool="$tools/appimagetool-$appimagetool_version-x86_64.AppImage"
if [ ! -x "$tool" ]; then
    curl -fsSL -o "$tool" \
        "https://github.com/AppImage/appimagetool/releases/download/$appimagetool_version/appimagetool-x86_64.AppImage"
    chmod +x "$tool"
fi
# Extract-and-run: no FUSE needed where the tool runs.
ARCH=x86_64 "$tool" --appimage-extract-and-run "$appdir" "$out/Deadlock-$version-x86_64.AppImage"

rm -rf "$appdir"
fi
ls -l "$out"
