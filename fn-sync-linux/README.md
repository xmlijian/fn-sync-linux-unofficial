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


## 日志与设置

日志日期筛选接受界面提交的空字符串和毫秒时间字符串。日志数量/天数为 0 表示不限；
修改保存规则立即执行清理，运行中每分钟维护一次。全局上传/下载限速修改后立即应用。
“导出日志文件”先选择保存位置，再生成 gzip 归档，内含 `sync-logs.jsonl` 和运行日志。
检查更新与自动更新当前不接入，界面会提示暂不支持。
`saved_sync_logs_are_visible_with_actual_ui_date_payload` 是只读实库检查，需要已有同步日志；
它会在临时目录验证实际导出内容。

暂停任务会停止当前执行器并取消未完成传输；继续时重新创建执行器，从保存的基线重新核对文件。已提交给 NAS 的服务端操作可能完成收尾，不进行回滚。界面操作后立即刷新状态，并每秒更新任务列表。

## 按需同步（open_cloud）

官方客户端在 Windows 用内核 CfAPI（cldflt.sys + provider.exe）、macOS 用 FileProvider 实现
“文件支持按需同步”；官方 Linux 明确不支持（界面文案 “Linux 暂不支持按需同步”）。
本实现用 **FUSE3 用户态挂载**等价复刻该语义（`src-tauri/src/tasks/ondemand/`）：

- 任务的 `pc_path` 即挂载点，目录实时映射 NAS 侧 `nas_path`（file.ls，1s TTL + 内核失效通知）
- 文件仅占位元数据（大小/时间来自 NAS，不占磁盘）；首次 open/read 才下载（等价官方 DemandWriter）
- 本地写入落缓存并异步上传（三步上传协议），flush 后由 worker 重试直至成功
- 远端变化（6s 根目录差异轮询）实时反映；远端已变化而本地基于旧版本修改 → NAS 侧保留双方（Conflicted Copy）
- 右键菜单：文件支持下载 / 释放 / 始终保留；文件夹支持整目录下载 / 释放 / 保留。文件夹的“保留”是一次性动作，只对点击时已有的文件生效，之后新增的文件不会自动保留
- 编辑器原子保存（临时文件 + rename）已支持：`.goutputstream-*` 等临时文件只留本地，改名为正式文件前不会上传
- 缓存 LRU 淘汰（默认上限 10 GiB，system_table `ondemand_cache_limit_bytes` 可覆盖），等价官方“释放电脑磁盘”
- 崩溃/重启后未上传的修改自动补传（缓存目录 `.meta.json`）
- 删除任务时清理缓存；存在未上传修改则保留并告警

运行时依赖：内核 fuse 模块 + `fuse3` 包（deb 已声明依赖）。卸载发生在：任务暂停/停止/删除、
切换 open_cloud、应用退出（RunEvent::Exit 兜底，不留死挂载点）。

真机自检：

```bash
fn-sync-client --selftest-ondemand <host[:port]> <user> <password>
# 期望输出 SELFTEST_ONDEMAND_OK
```

本地无 NAS 的端到端挂载测试（内存假后端 + 真实 FUSE）：

```bash
cd src-tauri && FN_SYNC_FUSE_TEST=1 cargo test --lib mount_read_write_delete_rename_roundtrip -- --ignored
```

前端 `official-ui/assets/index-*.js` 有最小补丁：仅放开 open_cloud 复选框的
`type()=="linux"` 禁用条件与提示文案两处，其余界面逻辑未改动。

## 排障记录（2026-11-21）

### 全部任务“同步失败”且详情页空白

两个独立问题叠加：

1. **会话被作废后不自愈**：NAS 侧 token 失效（他处重新登录踢下线 / 过期）后，任务 actor 在
   WebSocket 仍连接时不会重新鉴权，`file.ls` 持续报 `errno 4224`，每秒刷一条失败日志，
   任务状态卡在 ERROR。实测 errno 语义：`135168`=token 过期、`4224`=会话被服务端作废、
   `65534`=连接未鉴权。修复：`NasClient::rpc` 对这三类 errno 主动断开，actor 下一轮
   `client()` 重连并走 `restore_session`（`user.tokenLogin` 用 long_token 刷新）。
   恢复链路已用真实 NAS 验证：tokenLogin → authToken(新 token) → file.ls 全通。

2. **详情页（同步日志）空白**：官方界面的日志筛选在未选择时提交 `null` 或 `""`
   （taskId/status/actionCode/pageSize），后端严格 `i64` 反序列化拒绝整个请求
   （`invalid type: string "", expected i64`），每秒轮询全部失败 → 页面无数据。
   修复：`GetLogsParams` 的 action_code/status/task_id/page 统一走宽容解析
   （`deserialize_optional_integer`），空串/null → None。
