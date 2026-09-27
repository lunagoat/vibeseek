#!/usr/bin/env bash
# Build dist/vibeseek-x86_64.AppImage: a portable binary (glibc ≥ 2.28, no OpenSSL) wrapped
# in an AppImage. Needs network the first time (zig, cargo-zigbuild, appimagetool → .tools/).
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
TOOLS=$ROOT/.tools
mkdir -p "$TOOLS" dist

# Toolchain: zig as the linker lets us target an old glibc from a new distro.
if [ ! -x "$TOOLS/zig/zig" ]; then
    url=$(curl -s https://ziglang.org/download/index.json | python3 -c 'import json,sys;d=json.load(sys.stdin);v=sorted([k for k in d if k!="master"],key=lambda s:[int(x) for x in s.split(".")]);print(d[v[-1]]["x86_64-linux"]["tarball"])')
    mkdir -p "$TOOLS/zig" && curl -sL "$url" | tar -xJ -C "$TOOLS/zig" --strip-components=1
fi
[ -x "$TOOLS/bin/cargo-zigbuild" ] || cargo install cargo-zigbuild --root "$TOOLS" -q
if [ ! -x "$TOOLS/appimagetool" ]; then
    curl -sL -o "$TOOLS/appimagetool" https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
    chmod +x "$TOOLS/appimagetool"
fi
export PATH="$TOOLS/zig:$TOOLS/bin:$PATH"

cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.28
BIN=target/x86_64-unknown-linux-gnu/release/vibeseek

APPDIR=$(mktemp -d)/vibeseek.AppDir
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/256x256/apps"
install -m755 "$BIN" "$APPDIR/usr/bin/vibeseek"
install -m755 packaging/AppRun "$APPDIR/AppRun"
cp packaging/vibeseek.desktop "$APPDIR/vibeseek.desktop"
cp packaging/vibeseek.desktop "$APPDIR/usr/share/applications/"
rsvg-convert -w 256 -h 256 packaging/vibeseek.svg -o "$APPDIR/vibeseek.png"
cp "$APPDIR/vibeseek.png" "$APPDIR/usr/share/icons/hicolor/256x256/apps/vibeseek.png"
ln -sf vibeseek.png "$APPDIR/.DirIcon"

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
OUT="dist/vibeseek-$VERSION-x86_64.AppImage"
ARCH=x86_64 "$TOOLS/appimagetool" --appimage-extract-and-run --no-appstream "$APPDIR" "$OUT" >/dev/null
rm -rf "$(dirname "$APPDIR")"
echo "built $OUT ($(du -h "$OUT" | cut -f1))"
