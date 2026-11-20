#!/usr/bin/env bash
# fn-sync-linux deb 打包脚本（cargo-deb 路径；tauri bundle 为备选）
set -e
cd "$(dirname "$0")/src-tauri"
export GIT_HASH=${GIT_HASH:-$(git rev-parse --short HEAD 2>/dev/null || echo dev)}
command -v cargo-deb >/dev/null || cargo install cargo-deb --locked
cargo build --release
cargo deb --no-build -Z gz
ls -la target/debian/*.deb
