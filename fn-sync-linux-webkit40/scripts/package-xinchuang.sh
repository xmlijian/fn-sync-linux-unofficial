#!/usr/bin/env bash
# 把信创版（麒麟/UOS）的 deb、AppImage、校验和与发布说明汇总到
# artifacts/信创版/，便于整目录上传/分发。
#
# 交付文件名使用「信创_」前缀，便于用户一眼区分；包内 Debian 版本仍为
# ${VERSION}-${REVISION}（中文不能进版本字段，且需保持对旧 web40 包的升级路径）。
#
# 前置：./build-docker.sh（amd64）、./build-docker-arm64.sh（arm64）、
#       ./scripts/build-appimage.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$ROOT/artifacts"
OUT="$SRC/信创版"
VERSION="${VERSION:-0.2.3}"
REVISION="${REVISION:-xinchuang}"

mkdir -p "$OUT"

pairs=(
    "fn-sync-client_${VERSION}-${REVISION}_amd64.deb:xinchuang_fn-sync-client_${VERSION}_amd64.deb"
    "fn-sync-client_${VERSION}-${REVISION}_arm64.deb:xinchuang_fn-sync-client_${VERSION}_arm64.deb"
    "fn-sync-client-${VERSION}-${REVISION}-x86_64.AppImage:xinchuang_fn-sync-client-${VERSION}-x86_64.AppImage"
    "fn-sync-client-${VERSION}-${REVISION}-aarch64.AppImage:xinchuang_fn-sync-client-${VERSION}-aarch64.AppImage"
)
for pair in "${pairs[@]}"; do
    src="${pair%%:*}"
    dst="${pair#*:}"
    if [ ! -f "$SRC/$src" ]; then
        echo "缺少 $SRC/$src（先构建对应架构的包）" >&2
        exit 2
    fi
    cp -f "$SRC/$src" "$OUT/$dst"
done

# 清理上一版命名遗留的交付文件
find "$OUT" -maxdepth 1 -type f \( -name 'fn-sync-client*' -o -name '信创_*' \) -delete

(cd "$OUT" && sha256sum ./*.deb ./*.AppImage > SHA256SUMS.txt)
cp -f "$ROOT/社区发布说明-国产化版.md" "$OUT/"

echo "==> artifacts/信创版/"
ls -lh "$OUT"
