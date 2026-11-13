#!/usr/bin/env bash
# STUB: AppImage packaging for the next stage. Not tested.
# Requires `linuxdeploy` (preferred) or `appimagetool` on PATH.
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release --workspace
APPDIR=target/AppDir
rm -rf "$APPDIR"
install -Dm755 target/release/shadersweep      "$APPDIR/usr/bin/shadersweep"
install -Dm755 target/release/shadersweep-gui  "$APPDIR/usr/bin/shadersweep-gui"
install -Dm644 packaging/shadersweep.desktop   "$APPDIR/usr/share/applications/shadersweep.desktop"
install -Dm644 packaging/shadersweep.svg       "$APPDIR/usr/share/icons/hicolor/scalable/apps/shadersweep.svg"
install -Dm644 packaging/shadersweep.metainfo.xml "$APPDIR/usr/share/metainfo/io.github.shadersweep.shadersweep.metainfo.xml"
if command -v linuxdeploy >/dev/null; then
  linuxdeploy --appdir "$APPDIR" -d packaging/shadersweep.desktop -i packaging/shadersweep.svg --output appimage
elif command -v appimagetool >/dev/null; then
  ln -sf usr/share/applications/shadersweep.desktop "$APPDIR/shadersweep.desktop"
  ln -sf usr/share/icons/hicolor/scalable/apps/shadersweep.svg "$APPDIR/shadersweep.svg"
  printf '#!/bin/sh\nexec "$(dirname "$0")/usr/bin/shadersweep-gui" "$@"\n' > "$APPDIR/AppRun"; chmod +x "$APPDIR/AppRun"
  appimagetool "$APPDIR"
else
  echo "TODO: install linuxdeploy or appimagetool" >&2; exit 1
fi
