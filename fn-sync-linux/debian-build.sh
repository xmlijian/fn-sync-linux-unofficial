#!/usr/bin/env bash
# fn-sync-linux deb 打包脚本（cargo-deb 路径；tauri bundle 为备选）
set -e
cd "$(dirname "$0")/src-tauri"
export GIT_HASH=${GIT_HASH:-$(git rev-parse --short HEAD 2>/dev/null || echo dev)}
# deb 修订号：默认 20（晚于之前发布的 0.2.3-19，可正常升级）
DEB_REVISION=${DEB_REVISION:-20}
command -v cargo-deb >/dev/null || cargo install cargo-deb --locked
cargo build --release
cargo deb --no-build --deb-revision "$DEB_REVISION" -Z gz
ls -la target/debian/*.deb
