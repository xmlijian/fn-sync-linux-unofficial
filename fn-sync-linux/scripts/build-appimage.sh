#!/usr/bin/env bash
# 构建新系统版（Tauri 2 / webkit2gtk-4.1）AppImage。
#
# 默认按本机架构构建（x86_64 / aarch64）。交叉或在容器内构建时可覆盖：
#   ARCH=aarch64 BIN=.../target/aarch64-unknown-linux-gnu/release/fn-sync-client \
#     ./scripts/build-appimage.sh
#
# 运行依赖系统自带的 libwebkit2gtk-4.1 / libgtk-3 / libsoup-3.0 / fuse；
# 二进制在本机编译，因此和本机 glibc 基线一致。
#
# 不使用 appimagetool/linuxdeploy：runtime（AppImageKit 官方）+ squashfs(xz) 拼装。
# 产物：artifacts/fn-sync-client-<version>-<arch>.AppImage
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="${VERSION:-0.2.3}"
ARCH="${ARCH:-$(uname -m)}"
case "$ARCH" in
  x86_64|amd64) APPIMAGE_ARCH=x86_64 ;;
  aarch64|arm64) APPIMAGE_ARCH=aarch64 ;;
  *) echo "不支持的架构: $ARCH（可用: x86_64, aarch64）" >&2; exit 2 ;;
esac
BIN="${BIN:-$ROOT/src-tauri/target/release/fn-sync-client}"
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/fn-sync-appimage"

[ -x "$BIN" ] || { echo "缺少 $BIN，先执行: cd src-tauri && cargo build --release" >&2; exit 2; }

mkdir -p "$CACHE" "$ROOT/artifacts"
if [ ! -s "$CACHE/runtime-$APPIMAGE_ARCH" ]; then
    curl -sL --max-time 120 -o "$CACHE/runtime-$APPIMAGE_ARCH" \
        "https://github.com/AppImage/AppImageKit/releases/download/continuous/runtime-$APPIMAGE_ARCH"
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

OUT="$ROOT/artifacts/fn-sync-client-${VERSION}-${APPIMAGE_ARCH}.AppImage"
cat "$CACHE/runtime-$APPIMAGE_ARCH" "$WORK/app.squashfs" > "$OUT"
chmod 755 "$OUT"
echo "==> $OUT"
ls -lh "$OUT"
