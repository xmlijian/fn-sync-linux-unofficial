#!/usr/bin/env bash
# 构建 UOS/DDE 文件管理器扩展（x86_64，目标机 124 = UOS 20 / dde-file-manager 6.0.60）：
#   右键菜单（下载/释放/始终保留/取消保留）+ 文件状态角标。
#
# 交叉/兼容构建说明：
#   - 用 glibc 2.28 的 webkit40-build 容器编译（UOS 20 的 glibc 也是 2.28）；
#   - dfm-extension 是纯 C++ 接口，插件不链接 Qt，只链接 libdfm-extension.so.1；
#   - 头文件随仓库放在 plugins/dde/include/（来自 libdfm-extension-dev 6.0.58.91），
#     ABI 自 6.0 起稳定；
#   - 链接用的 libdfm-extension.so.1 不放进仓库，由脚本从容器内 /opt/uos-dfm/lib
#     读取。首次使用前从目标机拷一份：
#       scp test@<UOS机>:/usr/lib/x86_64-linux-gnu/libdfm-extension.so.6.0.60.9 /tmp/
#       docker cp /tmp/libdfm-extension.so.6.0.60.9 webkit40-build:/opt/uos-dfm/lib/
#
# 产物：plugins/dde/build/libfnsync-dde.so
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

docker start webkit40-build >/dev/null 2>&1 || true

docker exec -e ROOT="$ROOT" webkit40-build bash -lc '
set -e
SRC=/project/plugins/dde
B="$SRC/build"
rm -rf "$B"
mkdir -p "$B"

LIB=/opt/uos-dfm/lib
SO=$(ls "$LIB"/libdfm-extension.so* 2>/dev/null | head -1 || true)
if [ -z "$SO" ]; then
    echo "缺少 libdfm-extension.so.1：见脚本头部注释，从目标机拷到 $LIB" >&2
    exit 2
fi

g++ -std=c++14 -O2 -fPIC -shared -Wall -Wextra -Wno-unused-parameter \
    -I"$SRC" -I"$SRC/include" -I/project/plugins/third_party \
    -o "$B/libfnsync-dde.so" \
    "$SRC/fnsync_dde_status.cpp" \
    "$SRC/fnsync_dde_plugin.cpp" \
    "$SO"

echo "==> 产物"
ls -l "$B/libfnsync-dde.so"
echo "==> 依赖"
readelf -d "$B/libfnsync-dde.so" | grep NEEDED || true
echo "==> glibc 符号上限（必须 <= 2.28）"
objdump -T "$B/libfnsync-dde.so" | grep -oP "GLIBC_[0-9.]+" | sort -Vu | tail -1
echo "==> 导出符号"
nm -D --defined-only "$B/libfnsync-dde.so" | grep dfm_extension_
'