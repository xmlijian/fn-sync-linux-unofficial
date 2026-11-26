#!/usr/bin/env bash
# 在常驻编译容器 webkit40-build（debian:buster / glibc 2.28）里构建 x86_64 版本。
# 容器里已经装好 rust 工具链、cargo-deb 和 crates 缓存，直接复用，不重建镜像、不重建容器。
#
#   docker start webkit40-build
#   docker exec -it webkit40-build bash     # 手动进入排查
#
# 用法:
#   ./build-docker.sh          # release + deb
#   ./build-docker.sh test     # 先跑单元测试再构建
#   ./build-docker.sh shell    # 进入容器
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
CONTAINER="${CONTAINER:-webkit40-build}"
REVISION="${REVISION:-web40}"
GIT_HASH="${GIT_HASH:-dev}"
# 容器内 rustup 工具链位置
CARGO_PATH=/root/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin:/usr/local/cargo/bin

docker start "$CONTAINER" >/dev/null

run() { docker exec "$CONTAINER" bash -lc "export PATH=$CARGO_PATH:\$PATH; cd /project/src-tauri; $*"; }

case "${1:-build}" in
  shell)
    docker exec -it "$CONTAINER" bash -lc "export PATH=$CARGO_PATH:\$PATH; cd /project/src-tauri; exec bash"
    ;;
  test)
    run "GIT_HASH=$GIT_HASH cargo test --release 2>&1 | tail -25"
    ;&
  build)
    echo "==> UOS/DDE 文件管理器扩展（右键菜单 + 状态角标）"
    "$ROOT/scripts/build-dde-plugin.sh"
    echo "==> cargo build --release"
    run "GIT_HASH=$GIT_HASH cargo build --release 2>&1 | tail -10"
    echo "==> cargo deb（revision $REVISION，variant dde）"
    run "GIT_HASH=$GIT_HASH cargo deb --no-build --variant dde --deb-revision $REVISION -Z gz"
    echo "==> 产物"
    ls -la "$ROOT"/src-tauri/target/debian/*_amd64.deb
    echo "==> glibc 符号上限（必须 <= 2.28）"
    run "objdump -T target/release/fn-sync-client | grep -oP 'GLIBC_[0-9.]+' | sort -Vu | tail -1"
    ;;
  *)
    echo "未知参数: $1 (可用: build|test|shell)" >&2
    exit 2
    ;;
esac
