#!/usr/bin/env bash
# Build shadersweep-<version>-x86_64.AppImage (+ .sha256) into dist/.
#
# Contents: the CLI and GUI binaries, desktop entry, icon and AppStream metainfo.
# Nothing else is bundled on purpose: the binaries only link glibc, libgcc_s and
# fontconfig (DT_NEEDED), and winit dlopen()s libwayland-client, libxkbcommon and
# the X11/xcb libraries at runtime. All of these are on the AppImage excludelist
# (must come from the host, like GPU drivers), so the host's copies are used.
#
# appimagetool and the type2 runtime are pinned and verified by SHA-256.
set -euo pipefail
cd "$(dirname "$0")"

ID=io.github.legendarylolo318_cloud.shadersweep
ARCH=x86_64
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
TOOL_VER=1.9.1
TOOL_SHA=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
RT_VER=20251108
RT_SHA=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
TOOLS=target/appimage-tools
APPDIR=target/AppDir
OUT=dist/shadersweep-$VERSION-$ARCH.AppImage

fetch() { # url dest sha256
  if [ ! -f "$2" ] || ! echo "$3  $2" | sha256sum -c --status; then
    mkdir -p "$(dirname "$2")"
    curl -fSL --retry 3 -o "$2.part" "$1"
    echo "$3  $2.part" | sha256sum -c --quiet
    mv "$2.part" "$2"
  fi
}
fetch "https://github.com/AppImage/appimagetool/releases/download/$TOOL_VER/appimagetool-$ARCH.AppImage" \
  "$TOOLS/appimagetool-$TOOL_VER.AppImage" "$TOOL_SHA"
fetch "https://github.com/AppImage/type2-runtime/releases/download/$RT_VER/runtime-$ARCH" \
  "$TOOLS/runtime-$RT_VER-$ARCH" "$RT_SHA"
chmod +x "$TOOLS/appimagetool-$TOOL_VER.AppImage"

cargo build --release --locked --workspace

rm -rf "$APPDIR"
install -Dm755 target/release/shadersweep     "$APPDIR/usr/bin/shadersweep"
install -Dm755 target/release/shadersweep-gui "$APPDIR/usr/bin/shadersweep-gui"
install -Dm644 "packaging/$ID.desktop"        "$APPDIR/usr/share/applications/$ID.desktop"
install -Dm644 "packaging/$ID.metainfo.xml"   "$APPDIR/usr/share/metainfo/$ID.appdata.xml"
install -Dm644 packaging/shadersweep.svg      "$APPDIR/usr/share/icons/hicolor/scalable/apps/shadersweep.svg"
cp "packaging/$ID.desktop" "$APPDIR/$ID.desktop"
cp packaging/shadersweep.svg "$APPDIR/shadersweep.svg"
ln -s shadersweep.svg "$APPDIR/.DirIcon"
cat > "$APPDIR/AppRun" <<'APPRUN'
#!/bin/sh
# No arguments: GUI. Any arguments: CLI (e.g. `shadersweep-*.AppImage list`).
HERE="$(dirname "$(readlink -f "$0")")"
if [ "$#" -eq 0 ]; then
  exec "$HERE/usr/bin/shadersweep-gui"
fi
exec "$HERE/usr/bin/shadersweep" "$@"
APPRUN
chmod +x "$APPDIR/AppRun"

mkdir -p dist
rm -f "$OUT" "$OUT.sha256"
# --appimage-extract-and-run: works in containers/CI without FUSE.
ARCH=$ARCH "$TOOLS/appimagetool-$TOOL_VER.AppImage" --appimage-extract-and-run \
  --runtime-file "$TOOLS/runtime-$RT_VER-$ARCH" --no-appstream "$APPDIR" "$OUT"
(cd dist && sha256sum "$(basename "$OUT")" > "$(basename "$OUT").sha256")
ls -l "$OUT" "$OUT.sha256"
