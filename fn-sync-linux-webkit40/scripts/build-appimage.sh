#!/usr/bin/env bash
# 构建 xinchuang（Tauri 1 / webkit2gtk-4.0）AppImage：x86_64 与 aarch64。
#
# AppImage 是用户态单文件，不安装 Peony/DDE 文件管理器插件，所以 AppImage 版
# 没有右键菜单和状态角标（同步、托盘、按需同步正常）。
# 运行依赖系统自带的 libwebkit2gtk-4.0 / libgtk-3 / libsoup2.4 / fuse。
#
# 不使用 appimagetool/linuxdeploy，直接按 AppImage 规范拼装：
#   runtime（AppImageKit 官方 release）+ squashfs（mksquashfs）
# 这样 aarch64 包也不需要 qemu 或本机跑 arm 程序。
#
# 前置：两个架构的 release 二进制已经构建好（build-docker.sh / build-docker-arm64.sh）
# 产物：artifacts/fn-sync-client-<version>-xinchuang-<arch>.AppImage
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CONTAINER="${CONTAINER:-webkit40-build}"
VERSION="${VERSION:-0.2.3}"
REVISION="${REVISION:-xinchuang}"

docker start "$CONTAINER" >/dev/null 2>&1 || true

docker exec -e ROOT="$ROOT" -e VERSION="$VERSION" -e REVISION="$REVISION" "$CONTAINER" bash -lc '
set -e
export PATH=/usr/local/bin:/usr/bin:/bin:$PATH

RUNTIME_DIR=/opt/appimage
mkdir -p "$RUNTIME_DIR"
for a in x86_64 aarch64; do
    if [ ! -s "$RUNTIME_DIR/runtime-$a" ]; then
        curl -sL --max-time 120 -o "$RUNTIME_DIR/runtime-$a" \
            "https://github.com/AppImage/AppImageKit/releases/download/continuous/runtime-$a"
    fi
done

mkdir -p /project/artifacts
SRC=/project/scripts/desktop
DESKTOP=/project/src-tauri/debian/fn-sync-client.desktop
ICON=/project/src-tauri/icons/128x128.png

build_one() {
    local arch="$1" binary="$2"
    echo "==> AppImage $arch"
    test -x "$binary" || { echo "缺少 $arch 的 release 二进制：$binary" >&2; exit 2; }

    local work="/tmp/appimage-$arch"
    rm -rf "$work"
    local appdir="$work/AppDir"
    mkdir -p "$appdir/usr/bin" \
             "$appdir/usr/share/applications" \
             "$appdir/usr/share/icons/hicolor/128x128/apps"

    cp "$binary" "$appdir/usr/bin/fn-sync-client"
    chmod 755 "$appdir/usr/bin/fn-sync-client"
    cp "$SRC/fnsync-ctl" "$appdir/usr/bin/fnsync-ctl"
    chmod 755 "$appdir/usr/bin/fnsync-ctl"
    # ctl 会从自己所在目录 import fnsync_ipc，放同目录即可。
    cp "$SRC/fnsync_ipc.py" "$appdir/usr/bin/fnsync_ipc.py"
    cp "$DESKTOP" "$appdir/usr/share/applications/"
    cp "$ICON" "$appdir/usr/share/icons/hicolor/128x128/apps/fn-sync-client.png"
    cp "$DESKTOP" "$appdir/fn-sync-client.desktop"
    ln -sf usr/share/icons/hicolor/128x128/apps/fn-sync-client.png "$appdir/.DirIcon"

    cat > "$appdir/AppRun" <<"EOF"
#!/bin/sh
HERE=$(dirname "$(readlink -f "$0")")
export PATH="$HERE/usr/bin:$PATH"
export APPDIR="$HERE"
exec "$HERE/usr/bin/fn-sync-client" "$@"
EOF
    chmod 755 "$appdir/AppRun"

    # AppImage 官方 runtime 只认 xz/zlib（不认 squashfs 的 legacy lzma），用 xz。
    mksquashfs "$appdir" "$work/out.squashfs" -comp xz -noappend -no-progress -all-root >/dev/null

    local out="/project/artifacts/fn-sync-client-${VERSION}-${REVISION}-${arch}.AppImage"
    cat "$RUNTIME_DIR/runtime-$arch" "$work/out.squashfs" > "$out"
    chmod 755 "$out"
    echo "    -> $out ($(du -h "$out" | cut -f1))"
}

build_one x86_64  /project/src-tauri/target/release/fn-sync-client
build_one aarch64 /project/src-tauri/target/aarch64-unknown-linux-gnu/release/fn-sync-client

echo "==> 产物"
ls -lh /project/artifacts/*.AppImage
'