#!/usr/bin/env bash
# 把信创版（xinchuang：麒麟/UOS）的 deb、AppImage、校验和与发布说明汇总到
# artifacts/信创版/，便于整目录上传/分发。
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

for name in \
    "fn-sync-client_${VERSION}-${REVISION}_amd64.deb" \
    "fn-sync-client_${VERSION}-${REVISION}_arm64.deb" \
    "fn-sync-client-${VERSION}-${REVISION}-x86_64.AppImage" \
    "fn-sync-client-${VERSION}-${REVISION}-aarch64.AppImage"; do
    if [ ! -f "$SRC/$name" ]; then
        echo "缺少 $SRC/$name（先构建对应架构的包）" >&2
        exit 2
    fi
    cp -f "$SRC/$name" "$OUT/$name"
done

(cd "$OUT" && sha256sum ./*.deb ./*.AppImage > SHA256SUMS.txt)
cp -f "$ROOT/社区发布说明-国产化版.md" "$OUT/"

echo "==> artifacts/信创版/"
ls -lh "$OUT"
