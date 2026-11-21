# fn-sync-linux 开发

逆向自 fn-sync 0.2.3 Windows/macOS 客户端的 Linux (amd64) 实现。

## 构建

```bash
# 系统依赖（一次性）
sudo apt-get install -y libwebkit2gtk-4.1-dev libjavascriptcoregtk-4.1-dev \
  build-essential curl wget file libxdo-dev libssl-dev \
  libayatana-appindicator3-dev librsvg2-dev

# 前端
npm install
npm run build

# 出 deb 包（cargo-deb / tauri bundle）
cd src-tauri
cargo tauri build --bundles deb
# 产物: target/release/bundle/deb/fn-sync-client_0.2.3_amd64.deb
```

## 测试

```bash
cd src-tauri
cargo test           # 单元测试（协议/schema/过滤/冲突）
cargo clippy         # lint
```

## 结构

- `src-tauri/src/connect/` — NAS 连接层：authx 签名、WebSocket 客户端、HTTP 传输、json-v1 协议类型
- `src-tauri/src/service/` — SQLite（schema 与官方一致）：database/nas_account/tasks/logs
- `src-tauri/src/tasks/` — 同步引擎：actor、inotify watch、过滤规则、冲突处理
- `src-tauri/src/platform/` — Linux 平台：自启动、mDNS、网络监控
- `src/` — React 前端（复用官方语言包 zh/en/ja）
- `protocol-docs/PROTOCOL.md` — 逆向协议规格（实现前必读）

## 联调清单（需要真实 fnOS NAS）

1. authx sign 拼接顺序：改 `connect/authx.rs` 的 `SIGN_TEMPLATE`，抓包比对官方客户端
2. protobuf tag 编号：抓 `/sync/event/register` 首包校准 `connect/proto.rs`
3. ECDH 会话加密参数（AES-GCM nonce 格式）
4. 上传/下载 `/sync/{action}` 的具体子路径
5. `x-comm-action` 的 action 名集合

## 当前 Linux 实现与验证

实际打包加载 `official-ui/`（见 `src-tauri/tauri.conf.json`），修改 `src/` 后的 Vite 产物不会自动替换这套界面。
使用 `./debian-build.sh` 构建发布程序和 gzip 压缩的 deb 包；脚本会先重新编译，避免打包旧二进制。

普通双向、仅上传、仅下载通过保存的 OAuth 会话连接 NAS，目录查询和文件传输复用认证会话。
访问令牌过期时使用长期凭据续期，认证成功后保存新令牌。
单向同步遵循界面说明：删除源端文件时保留目标端副本；双向同步传播删除并保留修改冲突副本。
Linux 当前没有按需文件占位提供器，因此按需同步开关禁用，使用普通同步。

接口与回归检查：

```bash
node scripts/check-ipc-contracts.cjs
GIT_HASH=$(git rev-parse --short HEAD) cargo test --manifest-path src-tauri/Cargo.toml --lib
```

真实 NAS 测试使用保存的账户凭据，只在独立测试目录中读写，测试目录必须包含 `fn-sync-qa-`。
不要使用正常任务目录执行真实 NAS 测试。设置 `FN_SYNC_TEST_DB`、`FN_SYNC_TEST_ADDRESS`、
`FN_SYNC_TEST_REMOTE_ROOT` 后，以 `cargo test --lib -- --ignored --nocapture` 运行。
覆盖文件上传/下载/改名/删除、三种模式、目录过滤、冲突、暂停/继续、断线恢复、停止/重建和离线删除。

界面渲染检查用实际 Tauri IPC 测试生成的响应样例运行，不代替真实 NAS 测试：

```bash
FN_SYNC_UI_FIXTURES=/tmp/fn-ui-fixtures.json GIT_HASH=qa cargo test --manifest-path src-tauri/Cargo.toml --lib ui_task_creation_settings_logs_and_deletion_contracts
FN_SYNC_UI_FIXTURES=/tmp/fn-ui-fixtures.json node scripts/qa-ui-smoke.cjs
```

当前验证机器为 x86_64 Linux。麒麟/UOS 的指定版本、ARM/龙芯等目标架构仍需在对应系统上构建和运行验证。
