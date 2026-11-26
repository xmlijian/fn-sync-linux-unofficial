#!/usr/bin/env bash
# 在常驻交叉编译容器 webkit40-arm 里构建 aarch64 版本（麒麟 V10 SP1 飞腾/麒麟990 等）。
# 容器里已经装好：aarch64-linux-gnu 交叉工具链、arm64 sysroot(/opt/sysroot)、
# rustup + aarch64 target、cargo-deb 和 crates 缓存。直接复用，不重建镜像、不重建容器。
#
#   docker start webkit40-arm
#   docker exec -it webkit40-arm bash       # 手动进入排查
#
# 用法:
#   ./build-docker-arm64.sh          # release + deb
#   ./build-docker-arm64.sh test     # 先跑单元测试再构建
#   ./build-docker-arm64.sh shell    # 进入容器
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
CONTAINER="${CONTAINER:-webkit40-arm}"
TARGET=aarch64-unknown-linux-gnu
REVISION="${REVISION:-web40}"
GIT_HASH="${GIT_HASH:-dev}"

docker start "$CONTAINER" >/dev/null

run() {
  docker exec "$CONTAINER" bash -lc "export PATH=/root/.cargo/bin:\$PATH \
    PKG_CONFIG_SYSROOT_DIR=/opt/sysroot \
    PKG_CONFIG_LIBDIR=/opt/sysroot/usr/lib/aarch64-linux-gnu/pkgconfig:/opt/sysroot/usr/share/pkgconfig \
    PKG_CONFIG_ALLOW_CROSS=1; cd /project/src-tauri; $*"
}

case "${1:-build}" in
  shell)
    docker exec -it "$CONTAINER" bash -lc 'export PATH=/root/.cargo/bin:$PATH; cd /project/src-tauri; exec bash'
    ;;
  test)
    run "GIT_HASH=$GIT_HASH cargo test --release --target $TARGET --lib 2>&1 | tail -20"
    ;&
  build)
    echo "==> Peony 文件管理器插件（右键菜单 + 状态角标）"
    "$ROOT/scripts/build-peony-plugin.sh"
    echo "==> cargo build --release --target $TARGET"
    run "GIT_HASH=$GIT_HASH cargo build --release --target $TARGET 2>&1 | tail -10"
    echo "==> cargo deb（revision $REVISION，variant peony）"
    run "GIT_HASH=$GIT_HASH cargo deb --no-build --target $TARGET --variant peony --deb-revision $REVISION -Z gz"
    echo "==> 产物"
    ls -la "$ROOT"/src-tauri/target/debian/*_arm64.deb
    echo "==> glibc 符号上限（必须 <= 2.28）"
    run "aarch64-linux-gnu-objdump -T target/$TARGET/release/fn-sync-client \
      | grep -oP 'GLIBC_[0-9.]+' | sort -Vu | tail -1"
    ;;
  *)
    echo "未知参数: $1 (可用: build|test|shell)" >&2
    exit 2
    ;;
esac
