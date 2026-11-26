#!/usr/bin/env bash
# 构建新系统版（Tauri 2 / webkit2gtk-4.1）AppImage（x86_64，本机构建）。
#
# 运行依赖系统自带的 libwebkit2gtk-4.1 / libgtk-3 / libsoup-3.0 / fuse；
# 二进制在本机编译，因此和本机 glibc 基线一致（Ubuntu 24.04+ / Debian 13+ 等）。
#
# 不使用 appimagetool/linuxdeploy：runtime（AppImageKit 官方）+ squashfs(xz) 拼装。
# 产物：artifacts/fn-sync-client-<version>-x86_64.AppImage
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="${VERSION:-0.2.3}"
BIN="$ROOT/src-tauri/target/release/fn-sync-client"
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/fn-sync-appimage"

[ -x "$BIN" ] || { echo "缺少 $BIN，先执行: cd src-tauri && cargo build --release" >&2; exit 2; }

mkdir -p "$CACHE" "$ROOT/artifacts"
if [ ! -s "$CACHE/runtime-x86_64" ]; then
    curl -sL --max-time 120 -o "$CACHE/runtime-x86_64" \
        https://github.com/AppImage/AppImageKit/releases/download/continuous/runtime-x86_64
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
APPDIR="$WORK/AppDir"
mkdir -p "$APPDIR/usr/bin" \
         "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/128x128/apps"

cp "$BIN" "$APPDIR/usr/bin/fn-sync-client"
cp "$ROOT/scripts/desktop/fnsync-ctl" "$APPDIR/usr/bin/fnsync-ctl"
cp "$ROOT/scripts/desktop/fnsync_ipc.py" "$APPDIR/usr/bin/fnsync_ipc.py"
chmod 755 "$APPDIR/usr/bin/fn-sync-client" "$APPDIR/usr/bin/fnsync-ctl"
cp "$ROOT/src-tauri/debian/fn-sync-client.desktop" "$APPDIR/usr/share/applications/"
cp "$ROOT/src-tauri/icons/128x128.png" "$APPDIR/usr/share/icons/hicolor/128x128/apps/fn-sync-client.png"
cp "$ROOT/src-tauri/debian/fn-sync-client.desktop" "$APPDIR/fn-sync-client.desktop"
ln -sf usr/share/icons/hicolor/128x128/apps/fn-sync-client.png "$APPDIR/.DirIcon"

cat > "$APPDIR/AppRun" <<"EOF"
#!/bin/sh
HERE=$(dirname "$(readlink -f "$0")")
export PATH="$HERE/usr/bin:$PATH"
export APPDIR="$HERE"
exec "$HERE/usr/bin/fn-sync-client" "$@"
EOF
chmod 755 "$APPDIR/AppRun"

mksquashfs "$APPDIR" "$WORK/app.squashfs" -comp xz -noappend -no-progress -all-root >/dev/null

OUT="$ROOT/artifacts/fn-sync-client-${VERSION}-x86_64.AppImage"
cat "$CACHE/runtime-x86_64" "$WORK/app.squashfs" > "$OUT"
chmod 755 "$OUT"
echo "==> $OUT"
ls -lh "$OUT"
