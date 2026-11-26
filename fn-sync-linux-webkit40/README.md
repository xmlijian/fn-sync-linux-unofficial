# fn-sync-linux 国产化系统适配版（Tauri 1.x / webkit2gtk-4.0）

逆向自 fn-sync 0.2.3 Windows/macOS 客户端的 Linux 实现，**本目录是 Tauri 1.x 变体**：
面向麒麟 V10 SP1、UOS 20 等只有 `libwebkit2gtk-4.0`（libsoup2）运行库的国产化系统。
业务代码与本仓库的 Tauri 2 主工程完全一致（含按需同步/FUSE），差异仅在框架胶水层：

| 层 | Tauri 2 主工程 | 本目录（Tauri 1.x） |
|---|---|---|
| UI 运行库 | libwebkit2gtk-4.1（libsoup3） | libwebkit2gtk-4.0-37（libsoup2） |
| 插件 | tauri-plugin-dialog/shell/opener/autostart/os | v1 内置 dialog/shell + `official-ui/tauri1-shim.js` 桥接 |
| 前端 | `__TAURI_INTERNALS__`（v2） | shim 在页面脚本前注入 v2 兼容层，业务 bundle 不修改 |
| 托盘 | `TrayIconBuilder` | `SystemTray`（v1 内置） |
| 权限 | capabilities/*.json | `allowlist`（window/dialog/shell） |
| 打包依赖 | libwebkit2gtk-4.1-0 / libsoup-3.0 | libwebkit2gtk-4.0-37 / libsoup2.4-1 |

## 构建

必须用 **debian:buster（glibc 2.28）容器**构建——产物要在 UOS 20（glibc 2.28）
与麒麟 V10 SP1（glibc 2.31）上运行，宿主系统（glibc 2.39）编出来的二进制目标机跑不了。

构建用的容器已常驻保留，**不要删、不要重建**（cargo 缓存、交叉工具链、arm64 sysroot 都在容器里）：

| 容器 | 用途 | 内容 |
|---|---|---|
| `webkit40-build` | x86_64 构建 | rustup 工具链、cargo-deb、crates 缓存 |
| `webkit40-arm` | aarch64 交叉构建 | aarch64-linux-gnu 工具链、arm64 sysroot(`/opt/sysroot`)、aarch64 target、cargo-deb |

```bash
# x86_64（UOS 20）：会先构建 DDE 扩展，再用 --variant dde 打包
./build-docker.sh
# 产物: src-tauri/target/debian/fn-sync-client_0.2.3-xinchuang_amd64.deb

# aarch64（麒麟 V10 SP1 飞腾/麒麟990）：会先构建 Peony 插件，再用 --variant peony 打包
./build-docker-arm64.sh
# 产物: src-tauri/target/debian/fn-sync-client_0.2.3-xinchuang_arm64.deb

# 单独构建文件管理器插件
./scripts/build-dde-plugin.sh     # x86_64 → plugins/dde/build/libfnsync-dde.so
./scripts/build-peony-plugin.sh   # arm64  → plugins/peony/build/libfnsync-peony-*.so

# 构建 AppImage（复用上面两个架构的 release 二进制）
./scripts/build-appimage.sh
# 产物: artifacts/fn-sync-client-0.2.3-xinchuang-{x86_64,aarch64}.AppImage

# 汇总信创版交付物（2 个 deb + 2 个 AppImage + 校验和 + 发布说明）
./scripts/package-xinchuang.sh
# 输出目录: artifacts/信创版/

# 需要进容器手工排查：
docker start webkit40-build && docker exec -it webkit40-build bash
docker start webkit40-arm   && docker exec -it webkit40-arm bash

# 发布前自检（glibc 符号不得高于 2.28）
objdump -T src-tauri/target/release/fn-sync-client \
  | grep -oP 'GLIBC_[0-9.]+' | sort -Vu | tail -1
```

前端不需要 Node：直接使用已构建的 `official-ui/` 静态资源（含 `tauri1-shim.js`）。

## 离线安装（无外网目标机）

deb 的运行时依赖全部为国产化桌面系统的预装组件，`dpkg -i` 不联网即可安装：

| 依赖 | 麒麟 V10 SP1（飞腾 140 / 麒麟990 134） | UOS 20（124） |
|---|---|---|
| libwebkit2gtk-4.0-37 / libjavascriptcoregtk-4.0-18 | 2.38.6（预装） | 预装 |
| libgtk-3-0 / libsoup2.4-1 | 预装 | 预装 |
| libayatana-appindicator3-1 \| libappindicator3-1 | libappindicator3-1 12.10.1 | libayatana-appindicator3-1 0.5.3 |
| fuse3 \| fuse（提供 fusermount3/fusermount） | 134 仅 fuse（fusermount）；140 两者都有 | 有 |
| 二进制实际 NEEDED | 仅 webkit/gtk/soup/js/glib 等上述包 | 同左 |

实测：140（arm64）、134（arm64）、124（amd64）均 `dpkg -i` 直接成功，无缺失依赖；
`libfuse3` 不在依赖内（fuser 使用纯 Rust 挂载实现，运行时只需 fusermount）。

可选组件：`nautilus-python`（Nautilus 状态角标扩展）在麒麟/UOS 上通常未安装，
且两系统的文件管理器分别为 UKUI/Peony 与 DDE 文件管理器，不使用 Nautilus 扩展，属可选。

AppImage 免安装版（`scripts/build-appimage.sh`）按 AppImage 规范用
`runtime + squashfs(xz)` 拼装，两个架构都能在 x86 构建容器里完成，不需要 qemu。
运行依赖系统自带的 `libwebkit2gtk-4.0` / `libgtk-3` / `libsoup2.4` / fuse；
它不向系统目录写文件，所以**不含 Peony/DDE 插件**（没有右键菜单和角标）。

## 麒麟 Peony 文件管理器集成（arm64 包内置）

arm64 包（麒麟 V10 SP1）额外内置两个 Peony 插件，安装后自动注册：

| 插件 | 作用 |
|---|---|
| `libfnsync-peony-menu.so` | 右键菜单：下载到本地 / 释放空间 / 始终保留 / 取消保留（文件与文件夹各一套） |
| `libfnsync-peony-emblem.so` | 状态角标：灰云=未下载、绿云=已下载、蓝色=下载中、上箭头=上传中、橙色=已修改 |

- 插件用麒麟源的 **Qt 5.12.12** 构建（`scripts/build-peony-plugin.sh`）：麒麟 V10
  各批次 Qt 版本不同（134 = Qt 5.15.2 + Peony 3.20.4，140 = Qt 5.12.12 + Peony 3.26.0），
  Qt 二进制向后兼容，按最低版本构建两边都能加载（5.15 构建在 5.12 上会因
  `QtPrivate::argToQString` 等符号缺失导致文件管理器启动失败）。
- 插件直接装进 `/usr/lib/aarch64-linux-gnu/peony-extensions/`
  （Peony 3.20 / 3.26 的插件扫描目录），由 dpkg 负责安装/卸载。
- 装包后需**重启文件管理器或重新登录**才会加载插件（Peony 只在启动时扫描插件）。
- 角标随同步状态**自动刷新**（无需 F5）：插件把状态变化转成一次保持时间戳不变的
  `utimensat`，让 Peony 收到属性变更事件后重新查询角标。
- 排查：以 `FNSYNC_PEONY_DEBUG=1` 启动文件管理器后，日志在 `/tmp/fnsync-peony.log`。
- 实测：134（Qt 5.15.2 / Peony 3.20.4，wayland）与 140（Qt 5.12.12 / Peony 3.26.0，
  X11）均能加载插件、显示角标并弹出右键菜单。

## UOS/DDE 文件管理器集成（amd64 包内置）

amd64 包（UOS 20 专业版）额外内置 DDE 文件管理器扩展，安装后自动注册：

- 文件/文件夹右键 →「飞牛同步」子菜单：下载到本地 / 释放本地副本 /
  始终保留 / 取消始终保留（文件夹对应 下载/释放/保留/取消保留）；
- 状态角标：灰云=未下载、绿云=已下载、蓝=下载中、上箭头=上传中、橙=已修改；
- 角标随状态**自动刷新**：状态变化时插件对文件做一次保持时间戳的 `utimensat`，
  DDE 收到属性变更后重新查询角标，无需手动刷新。

实现与构建：

- 源码在 `plugins/dde/`（纯 C++，不依赖 Qt；`dfm-extension` 头文件随仓库放在
  `plugins/dde/include/`，JSON 解析用 `plugins/third_party/nlohmann/json.hpp`）；
- `scripts/build-dde-plugin.sh` 在 glibc 2.28 的 `webkit40-build` 容器里编译，
  链接 `libdfm-extension.so.1`（首次需从目标机拷到容器 `/opt/uos-dfm/lib/`，见脚本注释）；
- 插件直接装进 `/usr/lib/x86_64-linux-gnu/dde-file-manager/plugins/extensions/`，
  由 dpkg 负责安装/卸载（DDE 的扫描器不加载软链，所以必须是真实文件）。
- 装包后需重启文件管理器（DDE 的常驻进程是 `dde-file-manager -d`；注意 Linux
  comm 只保留前 15 个字符，`pkill -x dde-file-manager` 匹配不到）：
  `kill <pid>` 后在任务栏重新打开文件管理器即可。
- 排查：以 `FNSYNC_DDE_DEBUG=1` 启动 `dde-file-manager -d` 后，日志在 `/tmp/fnsync-dde.log`。
- 实测：124（UOS 20 / dde-file-manager 6.0.60.9）文件与文件夹菜单、灰/绿角标、
  状态变化自动刷新均正常；并用真实按需同步任务验证了「右键下载到本地」→
  图标自动 灰→蓝→绿 的完整链路。

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
本变体已启用按需同步（FUSE3 占位文件），见下文“按需同步（open_cloud）”。

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

已在麒麟 V10 SP1（arm64：飞腾 D2000 / 麒麟990）与 UOS 20 专业版（x86_64）真机验证；
龙芯等其它目标架构仍需在对应系统上构建和运行验证。


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
- 状态角标由守护进程事件推送即时刷新（300ms 轮询兜底）；右键操作成功不弹提示，只有失败才提示
- 路径解析失败会自动刷新目录；释放/下载的瞬时错误会自动重试
- 远端删除文件时会同步释放本地缓存并清理 inode；缓存根目录的孤儿副本会自动回收
- 单次远端列表缺失不会清理条目（连续两次缺失才确认），扩展也会短时保留旧角标，避免图标闪烁
- 编辑器原子保存（临时文件 + rename）已支持：`.goutputstream-*`、`文件名.~#N` 等临时文件只留本地，改名为正式文件前不会上传；`文件名.~#N` 备份不会被自动删除
- 缓存 LRU 淘汰（默认上限 10 GiB，system_table `ondemand_cache_limit_bytes` 可覆盖），等价官方“释放电脑磁盘”
- 崩溃/重启后未上传的修改自动补传（缓存目录 `.meta.json`）
- 上次异常退出（崩溃、被 kill）残留的死挂载会在下次挂载前自动回收：优先
  `fusermount3`，没有则回退 fuse2 的 `fusermount`（麒麟 134 只有 fuse2）；
  若控制 socket 仍有另一个客户端实例应答，则报错提示先退出该实例，不会抢占
- 删除任务时清理缓存；存在未上传修改则保留并告警

运行时依赖：内核 fuse 模块 + `fuse3` 或 `fuse` 包（提供 fusermount3/fusermount，deb 已声明 `fuse3 | fuse`）。
本变体使用 fuser 的纯 Rust 挂载实现（不链接 libfuse3），目标机无需 libfuse3 运行库。卸载发生在：任务暂停/停止/删除、
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

`official-ui/assets/index-*.css` 另有一处字体补丁：原官方字体栈
`Inter, -apple-system, ..., Microsoft YaHei, Helvetica Neue, Arial` 在麒麟/UOS 上
全部缺失，改为 `system-ui` 优先并兜底 `Noto Sans CJK SC` / `Source Han Sans SC`
（麒麟桌面字体是 Noto Sans CJK SC，UOS 是思源黑体），中英文与桌面观感一致。
字号同时整体 +1px（12→13、13→14、14→15），贴近桌面 11pt（14.7px）的观感。

## 排障记录（2026-11-25 / Tauri1 变体）

### 1. 登录授权卡住：授权窗登录成功也不回传凭据

NAS 前端（`assets/constants-*.js`）判定“桌面客户端授权窗”的条件是：

```js
m = !!window.__TAURI_INTERNALS__
h = m && window.__TAURI_INTERNALS__.metadata.currentWebview.label === 'async-oauth'
```

只有 `h` 为真，登录成功才会 `invoke('put_auth_result', {data})` 回传 token/secret。
`__TAURI_INTERNALS__` 是 **Tauri v2** 才自动注入到远程页面的对象；Tauri 1.x 没有，
导致页面按普通浏览器登录，主窗口 `handleAuth` 的 `once('oauth-result')` 永远不 resolve。
修复：`PROBE_SCRIPT`（由 oauth 代理注入到 NAS 登录页 `<head>` 之后、模块脚本之前）
补齐 `window.__TAURI_INTERNALS__`（`invoke` 走代理 `/__ipc`，label 固定 `async-oauth`）。

同时修正 `close_oauth_window`：窗口 label 是 `async-oauth`，原来查的是 `oauth`，
前端关闭登录弹窗或取消时清理不到授权窗。

### 2. 双重标题栏（麒麟 wayland 会话）

麒麟 V10 SP1 的 kwin_wayland 不实现 xdg-decoration，忽略 Tauri 的 `decorations:false`
另外画一条系统标题栏，与前端自绘标题栏叠加。134（飞腾/麒麟990）实测：

| 配置 | 结果 |
|---|---|
| wayland 默认 | 双标题栏，界面正常 |
| `GDK_BACKEND=x11` 单开 | 单标题栏，白屏（EGL 建不出上下文） |
| `GDK_BACKEND=x11` + `WEBKIT_DISABLE_COMPOSITING_MODE=1` | 单标题栏 + 界面完整 |

修复：`apply_render_env_defaults()` 在 wayland 会话（存在 `WAYLAND_DISPLAY`）下默认
`WEBKIT_DISABLE_COMPOSITING_MODE=1`，且若 Xwayland 可用（`DISPLAY` 存在）则默认
`GDK_BACKEND=x11`。用户显式设置这两个变量时以用户为准；纯 X11 桌面不受影响。

验证时注意：134 全屏截图里应用上方若出现一条“额外标题栏”，先 `wmctrl -lG` 看是否有
其它窗口叠在应用背后（实测是 `软通桥` 1000x680@460,177 露出的 23px 标题条），
单独抓窗口像素（`Gdk.pixbuf_get_from_window` / `import -window`）才是应用自己的绘制结果。
修复后 134（wayland 会话，走 Xwayland）与 140（X11）单窗口抓图均只有一个标题栏。

### 3. 选目录变成选文件（v2 插件命令参数形状）

官方前端用的是 v2 插件 API 的调用形状：

- `invoke("plugin:dialog|open", { options: { directory: true, ... } })`
- `invoke("plugin:window|set_size", { label, value: {...} })`

桥接层原来按扁平参数读（`a.directory`、`a.data`），读不到 → 目录选择框退化成文件选择框，
window 的 set_* 也全部失效。已统一改为先取 `options` / `value` 并兼容旧写法；
同时补齐 `plugin:app|version`（映射到后端 `get_app_version`）、`plugin:process|restart`、
webview/image/updater 等不存在于 v1 的命令的 no-op 处理。

### 4. 创建按需同步任务总是失败（提示挂载失败 / 重名 / 路径已共享）

现象：勾选“按需同步”后点创建，界面提示创建失败；再次点创建又提示任务名已存在，
换路径再试提示该路径已被共享，任务列表里却多出一条状态异常的任务。

根因：按需同步的 pc_path 是 FUSE 挂载点，而 fuse 不允许挂在非空目录上
（`fusermount: mountpoint is not empty`）。旧逻辑先 `add_task` 写库、再
`save_folder_filter_paths` 触发启动挂载；挂载失败时前端把整个创建流程报成失败，
但任务行已经落库（`status=5`），用户重试就会撞上后端的名重复/路径占用校验。

官方 macOS 客户端（dmg 内 `fn-sync-client` 二进制）的按需同步走 FileProvider，
同步目录固定在 `~/Library/CloudStorage/...`，由系统创建、初始为空，
从不允许用户选一个已有文件的目录；普通（非按需）同步才允许已有目录。

修复：`validate_ondemand_pc_path()` 在 `add_task`（创建时 open_cloud=1）和
`update_task`（0→1 切换时）校验电脑文件夹必须为空（不存在会自动创建），
非空直接返回中文提示且不写库；`OnDemandMount::start()` 再做一次兜底。
已在 124 上复现并验证：非空目录 → 明确报错、库里不留任务；空目录 → 正常创建并挂载。

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
