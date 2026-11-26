#!/usr/bin/env bash
# 构建麒麟 Peony 文件管理器插件（arm64）：
#   右键菜单（下载/释放/始终保留/取消保留）+ 文件状态角标。
#
# 在常驻的 webkit40-arm 交叉容器里编译，使用麒麟官方源里的
# Qt **5.12.12** 与 libpeony-dev 3.20.4 头文件/库。
#
# 为什么用 5.12 而不是更新的 5.15：麒麟 V10 SP1 各批次 Qt 版本不同
# （134 = Qt 5.15.2 + Peony 3.20.4，140 = Qt 5.12.12 + Peony 3.26.0）。
# Qt 二进制向后兼容——用最老的 5.12 构建，5.12 与 5.15 运行时都能加载；
# 反过来用 5.15 构建会在 5.12 上因缺少 QtPrivate::argToQString 等符号崩溃。
# 产物：plugins/peony/build/libfnsync-peony-{menu,emblem}.so
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

docker start webkit40-arm >/dev/null 2>&1 || true

docker exec -e ROOT="$ROOT" webkit40-arm bash -lc '
set -e
export PATH=/usr/local/bin:/usr/bin:/bin:$PATH

QT=/opt/kylin-plugin/qt512
PE=/opt/kylin-plugin/peony
QTV="5.12.12+dfsg-0kylin1k1.71update6fix0.1"
REPO="http://archive.kylinos.cn/kylin/KYLIN-ALL/pool/main"

fetch_deb() {
    local url="$1" dest="$2"
    local marker="$dest/.ok-$(basename "$url" .deb)"
    mkdir -p "$dest"
    if [ ! -e "$marker" ]; then
        curl -s -o /tmp/pkg.deb "$url"
        dpkg-deb -x /tmp/pkg.deb "$dest"
        touch "$marker"
    fi
}

# Qt 5.12.12（麒麟 KYLIN-ALL；140 用该版本，134 的 5.15 可向后兼容）
fetch_deb "$REPO/q/qtbase-opensource-src/qtbase5-dev_${QTV}_arm64.deb" "$QT"
for p in libqt5core5a libqt5gui5 libqt5widgets5 libqt5network5; do
    fetch_deb "$REPO/q/qtbase-opensource-src/${p}_${QTV}_arm64.deb" "$QT"
done
# Peony 3.20.4 接口头文件与运行时库（链接用）
PV="3.20.4.14-0k3.13update1.7"
fetch_deb "$REPO/p/peony/libpeony-dev_${PV}_arm64.deb" "$PE"
fetch_deb "$REPO/p/peony/libpeony3_${PV}_arm64.deb" "$PE"

SRC=/project/plugins/peony
B="$SRC/build"
rm -rf "$B"
mkdir -p "$B"

QTINC="$QT/usr/include/aarch64-linux-gnu/qt5"
INCLUDES="-I$SRC -I$PE/usr/include/peony-qt -I$QTINC -I$QTINC/QtCore -I$QTINC/QtGui -I$QTINC/QtWidgets -I$QTINC/QtNetwork"
CXX="aarch64-linux-gnu-g++ -std=c++14 -O2 -fPIC -Wall -Wextra -Wno-unused-parameter $INCLUDES"
LDFLAGS="-shared -Wl,-rpath-link=$QT/usr/lib/aarch64-linux-gnu -Wl,-rpath-link=$PE/usr/lib/aarch64-linux-gnu -L$QT/usr/lib/aarch64-linux-gnu -L$PE/usr/lib/aarch64-linux-gnu -lQt5Widgets -lQt5Gui -lQt5Network -lQt5Core"

# moc：使用容器内 moc 生成元对象（含插件元数据段）。
# buster 的 moc 是 qtchooser 包装，直接用真实路径。
MOC=/usr/lib/qt5/bin/moc
$MOC $INCLUDES -o "$B/moc_status.cpp"        "$SRC/fnsync_status_client.h"
$MOC $INCLUDES -o "$B/moc_menu_plugin.cpp"   "$SRC/fnsync_menu_plugin.h"
$MOC $INCLUDES -o "$B/moc_provider.cpp"      "$SRC/fnsync_emblem_provider.h"
$MOC $INCLUDES -o "$B/moc_emblem_plugin.cpp" "$SRC/fnsync_emblem_plugin.h"

$CXX $LDFLAGS -o "$B/libfnsync-peony-menu.so" \
    "$SRC/fnsync_status_client.cpp" \
    "$SRC/fnsync_menu_plugin.cpp" \
    "$B/moc_status.cpp" "$B/moc_menu_plugin.cpp" \
    -lpeony

$CXX $LDFLAGS -o "$B/libfnsync-peony-emblem.so" \
    "$SRC/fnsync_status_client.cpp" \
    "$SRC/fnsync_emblem_provider.cpp" \
    "$SRC/fnsync_emblem_plugin.cpp" \
    "$B/moc_status.cpp" "$B/moc_provider.cpp" "$B/moc_emblem_plugin.cpp" \
    -lpeony

echo "==> 产物"
ls -l "$B"/libfnsync-peony-*.so
echo "==> 依赖"
aarch64-linux-gnu-readelf -d "$B/libfnsync-peony-menu.so" | grep NEEDED
echo "==> glibc 符号上限（必须 <= 2.28）"
for so in "$B"/libfnsync-peony-*.so; do
    aarch64-linux-gnu-objdump -T "$so" | grep -oP "GLIBC_[0-9.]+" | sort -Vu | tail -1
done
'
