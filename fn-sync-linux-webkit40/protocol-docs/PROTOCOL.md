# FN Sync (飞牛同步) 协议规格 — 逆向自 0.2.3 Windows/macOS 客户端

> 证据来源：fn-sync-client 二进制静态分析（符号、字符串、格式串、反汇编）。
> 标注 **[未确认]** 的项需要在真实 fnOS NAS 上联调验证。

## 1. 网络端点

| 端点 | 用途 |
|---|---|
| `https://<nas>:5667`（HTTPS，默认端口 5667，可被 mDNS TXT `https=<port>` 覆盖） | NAS 同步服务 |
| `wss://<nas>:5667/sync/event/register` | WebSocket 事件通道（`?type=main` 亦见于字符串） |
| `https://<nas>:5667/sync/comm` + header `x-comm-action` | 通用命令（query_nas_os_edition 等） |
| `https://<nas>:5667/sync/{upload|download|...}` | 文件传输（路径按 action 拼接）**[未确认：具体子路径]** |
| `https://appversion.fnnas.com/api/v1/check-update/com.fn-sync-client/{target}/{arch}/{current_version}` | 更新检查 |
| `https://event.fnnas.com/api/v1/fe` | 遥测事件上报 |

连接类型：`ip`（IP:port 直连）、`host`（域名直连）或 `fnid`（FN Connect 中继）。
`fnid` 由后端请求 FN Connect 解析端点得到中继地址（`<id>.fnos.net:443`），后续 HTTP/WS 直连该地址。

## 2. 登录 / OAuth

1. 客户端打开 OAuth 窗口: `https://<address>/c/oauth`（HTTPS）。
2. 登录成功后页面回传（deep link / oauth-result 事件）:
   `token, long_token, secret, cookie, uid, username, machine_id, nas_id, pub_key, si, fnos_version, computer_name`
3. 账号持久化到 SQLite `nas_accounts`（字段见 §7）。

错误码（前端 error map）:
`connect_failed / websocket_stream_failed / websocket_handshake_failed / 401 / add_nas_account_failed / get_rsa_pub_key_failed / login_failed / get_secret_failed / get_host_name_failed / get_machine_id_failed / "syncserver not install or has stoped"`
另有字符串错误: `Version incorrect`（需要两端升级）。

## 3. authx 签名（HTTP 请求签名）

已恢复的常量（从二进制混淆解出，双重验证一致）:
```
PREFIX  = "NDzZTVxnRKP8Z0jXg1VAMonaG8akvh"   (30 字节)
API_KEY = "zIGtkc3dqZnJpd29qZXJqa2w7c"       (26 字节)
```

请求 query（build_authx 格式串恢复）:
```
nonce={u32}&timestamp={ms}&sign={md5hex}
```
- nonce: `rand(100_000..1_000_000)` u32
- timestamp: 本地时间毫秒
- sign: `md5("{p1}_{p2}_{p3}_{p4}_{p5}")` 小写 hex —— 格式串显示 4 个 `_` 分隔 5 个参数，
  参数含 PREFIX、API_KEY、请求相关字符串（路径/uid/token）**[未确认：参数精确顺序]**。
  实现为可配置拼接模板，联调时用真实 NAS 校验后锁定。

请求头（transport/client.rs 恢复）:
```
authx: <如上 query>
x-session-id, x-sign
x-magic, x-file, x-trace-id, x-ops-hash, x-uid, x-watch-root, x-sync-mode,
x-nonce, x-user-token, x-bodylen, x-sync-base-version, x-checksum,
x-file-ctx-filesize, x-base-version, x-src, x-dst, x-travel-type, x-check-acl,
x-agent, x-version, x-need-rsync, x-path-generation, x-diffmd5, x-sync-content-version
Cookie: mode=relay          (中继模式)
User-Agent / x-agent
content-type: application/octet-stream
```

## 4. WebSocket 事件协议 (`/sync/event/register`)

握手:
1. HTTP Upgrade，头部 `x-session-id`、`x-sign`，Cookie `mode=relay`（中继时）。
2. ECDH 密钥交换（X25519，公钥 32 字节）: 消息 `HandShakeReq{ClientPubKey}` →
   `HandShakeResp{ServerPubKey, has_exists, nasid, ...}`，之后会话加密。
3. 服务端经响应头/消息给出 `x-server-settings`、版本号（upgrade response server version / sdk version），
   版本比对失败 → `Version incorrect`。
4. wire_format 编号出现在握手完成日志: `handshake completed: nasid=.., wire_format=.., svr=.., server settings: ..`

消息封装: `WsMessage { connid, action: oneof }`，JSON-v1 与 protobuf 双格式（服务端旧版用 json-v1，
检测头部缺失时 "treating websocket peer as legacy json-v1 server"）。

JSON-v1 动作（serde 恢复，tuple variants）:
- `WsAction::CtlAction(action, ...)` / `WsAction::NasEvent(...)` / `WsAction::HandShakeResp(...)`
- `RespAction::AddWatch / DelWatch / FilterRuleChange / RefreshTokenResp / HandshakePerm`
- `FileEvent::Create / Modify / Remove / Rename`
- `SyncVersion { major, minor, patch }`
- `TravelNode`, `TravelFileType::File/Dir`

客户端→服务端控制消息:
- `CtlAddWatch { ops_hash, path, rule: FilterRule, dir? }` — Debug 串 "AddWatch: ops_hash: , path: , rule: , dir:"
- `CtlDelWatch { ops_hash, path }`
- `CtlFilterRuleChange { ops_hash, path, rule, dir }`
- `CtlRefreshToken`
- `FilterRule { only_dir?, ignore_dotfiles, ignore_extensions, size_lower_bound, size_upper_bound }`
  （模式枚举 `UpOnly / DownOnly / Both`）

服务端→客户端:
- `MsgNasEvent { events: [PbFileEvent] }`
- `PbFileEvent.event` oneof:
  - `Create { is_dir, fmtime, file_size }`
  - `Modify { allow_change_action, ... }`
  - `Remove { ... }`
  - `Rename { src_path, dst_path, ops_hash }`
- `ServerSettings { schema_version, modify_missing_as_create_default }`
- `TravelFileAttrs { ctmt, deleted, zchksum, ... }`

**[未确认]**: protobuf 各字段的 tag 编号（prost 未嵌 FileDescriptorSet）。
实现时先用 JSON-v1（服务端兼容），或通过首次抓包校准 proto 定义。

## 5. NAS 文件 RPC（走 WebSocket 会话的 JSON 消息）

前端经 `send_message_from_connection_id(conn_id, msg, is_hmac_sha256)` 发送:
```json
{ "req": "file.ls",      "reqid": "<hex ts+counter>", "path": "..." }
{ "req": "file.lsDir",   "reqid": "..." }
{ "req": "file.team.lsDir", "reqid": "..." }
{ "req": "file.mkdir",   "reqid": "...", ... }
```
响应: `{ "files": [ { name, dir, v(olume), size, ... } ] }`

`file.ls` / `file.lsDir` 实测（fnOS 0.20.12 / trim 1.2.1010）**回两帧、reqid 相同**：
```
{ "files": [...], "uver": <n>, "reqid": "<id>" }   ← 数据帧（先到）
{ "uver": <n>, "reqid": "<id>", "result": "succ" } ← 确认帧（后到，无 files）
```
客户端必须按 reqid 等待含 `files` 的数据帧；确认帧不是目录内容。
若把确认帧当成回包，会把目录解析成**空目录**（进而误删/误清缓存）。
`file.download` 以含 `download` 的帧为准（也可能与结果确认帧分开到达，见 14.5）。
只有 `user.login`（`encrypted` 信封）的成功回包不回显 reqid。

团队目录路径: `vol{v}/@team/{name}`。
reqid 生成: `hex(unix_ts,8) + hex(counter,4) + "0000000000000000"`。

## 6. 同步引擎

- 模式: `bisynchronous` / `uploadonly` / `downloadonly`（DB `tasks.mode`）
- 本地事件: inotify（Windows: ReadDirectoryChangesW，macOS: FSEvents）→ 与 NAS 事件合并到任务 actor
- 初始同步: tree BFS（`tree_bfs.rs`），服务端 travel 协议（`/sync/` + `x-travel-type`，`BtDiff/BtRaw/BtEmpty`）
- 文件比较: 本地文件签名（fast_rsync rolling hash）+ `x-base-version` 增量（rsync diff，`x-diffmd5`）
- 传输: 分块上传（`x-bodylen`、`x-file-ctx-filesize`、`x-checksum`、断点续传 "local resume checksum not eq server resume checksum, offset will be reset to 0"）
- 下载: 直接下载或 GenDiff 增量（"send GenDiff failed return 502 / nodiffs / succ down"）
- 冲突: 重命名为 `原名 - Conflicted Copy %Y-%m-%d %H-%M-%S`（" - Conflicted Copy " 格式串，conflict handler）
- 忽略规则（内置正则，恢复自字符串）:
  `.*|^Icon\r$|.*sync_temp_dir.*|.*FNSYNC.*|.*\.tmp$|.*\.temp$|^#.*` 以及 `*.download` 临时文件目录
- base version 存储: `base_versions.sqlite`（`store_meta` 表, `local_root/remote_root/remote_path`）

任务状态机（TaskStatus，DB `tasks.status`）:
```
1 INIT  2 SYNCING  3 PAUSED  4 COMPLETED  5 FAILED  6 SEGMENT_COMPLETED
7 ERROR 8 IDLE  9 ACCOUNT_ERROR  10 FAILED_LOCAL_DIR  11 FAILED_NAS_DIR
12 VERSION_INCORRECT  14 NAS_NO_ACL_SUPPORT  15 NAS_NO_PERM  16 NAS_FILE_NOT_EXISTS
17 SECURITY_CODE_ERROR
```

日志动作码（ActionCode）: `1 Upload / 2 Download / 3 Delete / 4 Rename / 5 Exception / 6 MetaCreate / 7 Move`
状态: `0 failed / 1 success`；错误码见前端 error map（1001..9999, 1000031..1000046）。

## 7. SQLite schema（database.sqlite，log.sqlite 同 logs 表）

```sql
-- system_table
CREATE TABLE IF NOT EXISTS system_table (
  key VARCHAR PRIMARY KEY ON CONFLICT IGNORE,
  value VARCHAR NOT NULL
);
INSERT INTO system_table (key, value) VALUES
  ('is_complete_first_run','0'), ('auto_update','1'),
  ('log_save_day','360'), ('log_save_size','1000000');
-- 另有 'language' 键

-- nas_accounts
CREATE TABLE IF NOT EXISTS nas_accounts (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  nas_id TEXT NOT NULL, uid INTEGER NOT NULL,
  nas_name TEXT NOT NULL, address TEXT NOT NULL, username TEXT NOT NULL,
  conn_type TEXT NOT NULL, secret_data TEXT NOT NULL DEFAULT '',
  long_token TEXT NOT NULL DEFAULT '', token TEXT NOT NULL DEFAULT '',
  failed_code INTEGER NOT NULL DEFAULT 0,
  pub_key TEXT NOT NULL, secret TEXT NOT NULL, si TEXT NOT NULL,
  computer_name TEXT DEFAULT '', fnos_version TEXT DEFAULT '',
  cookie TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);

-- tasks
CREATE TABLE IF NOT EXISTS tasks (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  nas_acc_id INTEGER NOT NULL,
  name TEXT NOT NULL, address TEXT NOT NULL, conn_type TEXT NOT NULL,
  uuid TEXT NOT NULL, identifier TEXT NOT NULL,
  mode TEXT NOT NULL,           -- bisynchronous | uploadonly | downloadonly
  nas_path TEXT NOT NULL, pc_path TEXT NOT NULL,
  status INTEGER NOT NULL,
  is_init_sync INTEGER NOT NULL DEFAULT 0,
  open_cloud INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);

-- task_filter_rules
CREATE TABLE IF NOT EXISTS task_filter_rules (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  task_id INTEGER NOT NULL,
  ignore_dotfiles INTEGER NOT NULL,
  enable_ignore_ext INTEGER NOT NULL,
  ignore_extensions TEXT,        -- "log,tmp"
  enable_size_lower INTEGER NOT NULL, size_lower_bound INTEGER,
  enable_size_upper INTEGER NOT NULL, size_upper_bound INTEGER,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);

-- logs（log.sqlite）
CREATE TABLE IF NOT EXISTS logs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  task_id INTEGER NOT NULL, is_dir INTEGER NOT NULL,
  name TEXT NOT NULL, path TEXT NOT NULL, sync_folder TEXT NOT NULL,
  action_code INTEGER NOT NULL, status INTEGER NOT NULL,
  failed_code INTEGER NOT NULL DEFAULT 0, failed_text TEXT,
  payload TEXT, created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_logs_created_at ON logs(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_logs_action_code ON logs(action_code);
CREATE INDEX IF NOT EXISTS idx_logs_name ON logs(name);
CREATE INDEX IF NOT EXISTS idx_logs_status ON logs(status);
-- 迁移: ALTER TABLE tasks ADD COLUMN open_cloud...
```

任务标识: `identifier = "tieren.SyncFile-{username}-{address}"`，`uuid = Date.now()`。

## 8. mDNS

服务类型 `_trim._tcp.local.`，TXT 记录 `https=5667`（缺失则默认 5667）。

## 9. 更新

tauri-plugin-updater 2.9.0，minisign 公钥 fingerprint `802248AEBF758891`，
端点 `https://appversion.fnnas.com/api/v1/check-update/com.fn-sync-client/{target}/{arch}/{current_version}`。

## 10. Tauri 命令清单（前端 invoke 全集，已恢复 81 个）

get_git_hash, app_quit, app_restart, get_own_pid, is_webview_debug, get_traffic_query,
set_sync_sdk_locale, open_browser, get_app_log_dir, export_log_file, connect_nas,
connect_nas_by_auth, close_oauth_window, create_nas_client, get_addr_type,
send_message_from_connection_id, send_message_from_temp_conn, delete_temp_conn,
delete_nas_client, get_all_nas_accounts, add_nas_account, delete_nas_account,
get_nas_account_by_address_and_username, get_nas_account_by_id, get_nas_account_by_nas_id,
update_nas_account_by_auth, update_nas_account_password, update_nas_conn_type_by_id,
check_trim_address, check_oauth_security_code, open_oauth_window, put_auth_result,
get_device_id, get_encrypt_data, get_all_system_table, update_system_table,
add_task, update_task, delete_task, get_task_by_id, get_all_tasks, get_task_count,
get_next_task_id, get_tasks_id_and_status_by_nas_acc_id, get_task_status_summary,
get_global_tasks_status, pause_task, resume_task, reload_task, renew_task,
update_task_status, remove_tasks_from_map, clear_all_data_by_task_id,
get_completed_items, get_extended_logs, get_logs_by_size, query_logs, add_log,
clear_all_logs, check_and_clear_logs, export_log_file, cmd_get_folder_list,
cmd_get_folder_list_with_params, cmd_get_folder_layer, cmd_get_folder_layer_with_params,
cmd_set_first_login_state, get_filter_rule_by_task_id, save_folder_filter_paths,
get_folder_filter_paths, delete_folder_filter_file, validate_app_name,
validate_task_nas_dir_exists, is_path_sync_root_command, is_external_device_path,
get_nodes_with_null_field, get_sync_change_page, list_file_domains, add_file_domain,
remove_file_domain, edit_open_cloud, reveal_item_in_dir, get_macos_cloud_pc_path,
get_git_hash

---

# 11. 实测校准（对真实 NAS 192.168.1.100，2026-11-20）

用真实 fnOS NAS 逐项验证了上述推断，记录**确认事实**与**未解阻塞点**。

## 11.1 已确认

### TLS
- NAS 使用自签证书。webkit2gtk 必须**在窗口自身 WebContext** 上设置
  `TLSErrorsPolicy::Ignore`（wry 为每个窗口建独立 WebContext，
  全局 `WebContext::default()` 无效）。
- 验证：`--selftest-url https://192.168.1.100:5667/c/oauth` → `SELFTEST_OK`。

### 同步服务端点存活
- `/sync/comm` 返回 **400**（不是 502）→ NAS 端同步服务已运行。
- 同步服务未安装/未启动时 nginx 返回 **502**，官方错误串
  `syncserver not install or has stoped`。
- `/sync/event/register` 同样 400。

### /sync/comm 请求头校验链（按顺序）
```
x-watch-root → x-file → x-uid → x-connid → x-ops-hash
→ x-comm-action → x-sign → "verify sign failed: ..."
```
缺哪个就报哪个。

### x-sign 格式（已确认）
```
x-sign: nonce=<纯数字>&timestamp=<纯数字>&sign=<hex>
```
错误反馈精确：
| 发送内容 | 服务端响应 |
|---|---|
| `nonce=abc&timestamp=1&sign=ff` | `verify sign failed: missing nonce in sign query`（nonce 必须数字） |
| `nonce=123456` | `verify sign failed: missing timestamp in sign query` |
| `nonce=..&timestamp=..` | `verify sign failed: missing sign in sign query` |
| 三段齐全但值错 | `invalid sign` |

→ 与客户端 `gen_authx` 恢复的格式串 `nonce={}&timestamp={}&sign={}` **完全一致**。

### 签名算法（部分确认，密钥未知）
- `gen_authx`（`sync_sdk::transport::client::NasConnOps`）格式串为
  `format!("{}_{}_{}_{}_{}", …)` → md5 hex → 填入 sign。
- 常量 `PREFIX`/`API_KEY` 属于 **遥测** 路径
  （`trim_sync_client_lib::service::utils::gen_authx_header` +
  `event_track::generate_authx_for_post_json`，用于 `event.fnnas.com`）。
- 对真实 NAS 穷举 **1440 组** PREFIX/API_KEY/nonce/timestamp/path/uid 排列组合，
  全部返回 `invalid sign` → **签名密钥来自登录后的 `secret`/`token`**，
  必须先登录才能签 `/sync/comm`。

### NAS 通用 REST API（无需 token 即可访问的部分）
| 端点 | 说明 |
|---|---|
| `GET /license/v1/device/baseInfo` | 返回 `{"code":0,"data":{displayName,vendor,productName,serialNumber,isDiy}}` ✅ |
| `GET /license/v1/guarantee/info` | 同上族 |
| `GET /app-center/openapi/v1/app/detail` | 应用中心，需参数/鉴权 |
| `POST /oauthapi/authorize` | OAuth 授权，需 `client_id/redirect_uri/response_type/token` |
| `POST /oauthapi/token` | `{"code":11001,"msg":"invalid request"}` / 带 grant_type 时 `11002 authentication failed` |
| `POST /oauthapi/refresh` | `{"code":15001,"msg":"invalid request"}` |

`/oauthapi/authorize` 错误语义：
```
{}                                          → 10001 parameters client_id/redirect_uri/response_type/token cannot be empty
{client_id:"fn-sync-client",...}            → 10002 invalid client_id
```
→ 存在 `token` 参数说明流程是：**用户先在 Web 登录拿 token → authorize 换 code → token 换 access_token**。

### 局域网发现（mDNS）
`_trim._tcp.local.` TXT 实测：
```
machineId=2f78cca96fb947208d9fdc8d190de3fb8001fdb2
inited=1; rev=1.2.1010; https=5667; modelId=0x0A9880
host=nas.local  port=5666(HTTP) / 5667(HTTPS)
```

## 11.2 阻塞点：`/c/oauth` 在该 NAS 上不存在

官方客户端二进制中确认（x86-64 反汇编，两处 xref，`lea` 直接加载该字面量）：
```
{address} + "/c/oauth"   ← 客户端用它打开“登录授权”窗口
```
且客户端二进制内**不存在** `client_id` / `redirect_uri` / `grant_type` /
`authorization_code` 等任何 OAuth 参数字样 → **授权页自己完成全部 API 交互，
再把结果回传给客户端**（`oauth-result` 事件）。

对该 NAS 实测：
| 路径 | 结果 |
|---|---|
| `/c/oauth` | 404 |
| `/c/oauth/`、`/c/oauth/authorize`、`/C/oauth`、`/c/OAuth` | 404 |
| `/fn-sync/oauth.html`、`/fn-sync/c/oauth`、`/sync/c/oauth`、`/trim/c/oauth` | 404 |
| `/c/<6+ 位字母数字>`（如 `/c/oauth2`、`/c/abcdef`） | 302 → `/`（短链重定向规则，非 oauth） |
| `/oauth`、`/signin`、`/login` | 200（**通用 NAS 登录页**，与 oauth 无关） |
| `/biz/oppo/login` | 200（NAS SPA 登录路由，见 `route-map` chunk） |
| 443 端口 `/c/oauth` | 302 → `https://host:5667/c/oauth`（仅端口跳转） |

**结论**：NAS 端「飞牛同步」App（rev 1.2.1010）未提供客户端期望的 `oauth` 页面。
官方客户端 0.2.3（`fnnas.com/download?key=fn-sync-client` 当前唯一版本，
仅 exe/dmg，**无 Linux 包**）在同一 NAS 上也会同样 404。

## 11.3 下一步（需 NAS 侧配合）

1. **确认 NAS 端「飞牛同步」App 版本**（应用中心），必要时更新到最新。
2. **用官方 Windows 客户端在该 NAS 上试连**，作为对照：
   - 若同样 404 → NAS 端 App 与客户端版本不匹配（非我方实现问题）
   - 若能登录 → 用 mitmproxy 抓取其请求，即可拿到 `client_id` 与真实 OAuth 流程
3. 拿到 `client_id` 后，我方实现可直接走
   `POST /oauthapi/authorize`（带用户 token）→ `code` → `POST /oauthapi/token`
   → 获得 access_token → 再用 token/secret 签 `/sync/comm`。

## 11.4 NAS 通用 API 体系（实测补充）

### 令牌存储（NAS 前端）
```
localStorage['fnos-token']       // 登录令牌
localStorage['fnos-long-token']  // 长效令牌
Cookie: fnos-token=...
请求头: Authorization: trim <fnos-token>
```

### 已确认的免鉴权端点
```
GET  /license/v1/device/baseInfo   → {"code":0,"data":{displayName,vendor,productName,serialNumber,isDiy}}
```

### 需要令牌的端点（无令牌均 401 或 "invalid token"）
```
GET  /app-center/openapi/v1/app/authorization-paths
GET  /app-center/openapi/v1/app/allow-user-authorization-paths
GET  /app-center/openapi/v1/user/authorization-paths
POST /cgi/<appcgi.模块.v1.方法>      ← 统一网关，方法名见 NAS 前端 bundle
POST /oauthapi/authorize            → 需 client_id/redirect_uri/response_type/token
POST /oauthapi/token                → 11001 invalid request / 11002 authentication failed
POST /oauthapi/refresh              → 15001 invalid request
```

### 端口与路由（实测）
| 端口 | 行为 |
|---|---|
| 80 / 443 | 302 → `https://host:5666` 或 `:5667` |
| 5666 | nginx，NAS Web（HTTP），同样有 `/sync/comm`、`/fn-sync/` |
| 5667 | nginx，NAS Web（HTTPS，默认） |
| 8080 | 「一键安装门户」静态页 |

nginx `location /c/` 是**短链重定向**：`/c/<≥6位字母数字>` → 302 `/`；
`/c/oauth`（5 位）→ 404。这就是 `/c/oauth` 不存在于该 NAS 的直接原因。

### 实测发现：网页登录替代路径
NAS 自带登录页 `/biz/oppo/login` 可用（200）。登录后 token 落在
`localStorage['fnos-token']`。因此 Linux 客户端可实现：
1. 打开 `/biz/oppo/login`
2. 注入轮询脚本读取 `fnos-token`
3. 跳转自定义 scheme `fn-sync-login://auth?token=...`，由 on_navigation 截获
4. 携带 `Authorization: trim <token>` 调用 NAS API 补齐凭据

已实现：`open_web_login_window` / `probe_nas_with_token` 命令 + 登录页「使用网页登录」按钮。
`probe_nas_with_token` 会逐个试探 6 个 NAS 端点并记录状态码与响应，
用于确定该 token 能解锁哪些接口（进而拿到 uid/username/machine_id 与同步凭据）。

---

# 12. 抓包 + 反编译实测结论（2026-11-20，决定性证据）

方法：把官方 **Windows 客户端 0.2.3** 装进 Win10 虚拟机，连本机假 NAS
（`https://192.168.1.101:5667`，自签证书 + 全量请求记录），同时用
Ghidra 12.1.4 对 macOS 版二进制按地址强制反编译。

## 12.1 真实请求格式（抓包，已确认）

```
GET /sync/comm HTTP/1.1
x-watch-root: Lw==                 ← base64("/")
x-file:       Lw==                 ← base64("/")
x-connid: 14452309325630534158     ← u64 随机
x-trace-id: 0                      ← 每请求递增（0, 2, ...）
x-ops-hash: 0
x-comm-action: query_nas_os_edition
x-sign: nonce=8120301&timestamp=1795158318931&sign=debb01bf...b077
```

**关键纠正（相比早期推断）**：
- `x-watch-root` / `x-file` 是 **base64**，不是明文路径
- `x-connid` 是 u64 随机值，不是 1
- 存在早期未发现的头 `x-trace-id`
- 首次探测**不带** `x-uid` / `x-user-token`
- `sign` 是 **64 位 hex = SHA-256 系**，不是 md5

## 12.2 客户端 OAuth 通道（Info.plist 确认）

```
CFBundleURLSchemes = ["fnsync"]     ← deep-link 回传通道 fnsync://
CFBundleIdentifier = com.fn-sync-client.app
```
客户端打开 `{address}/c/oauth`，授权页完成后经 `fnsync://` 回传，
客户端发 `oauth-result` 事件。**客户端二进制内不含 client_id / grant_type
等任何 OAuth 参数字样** → 授权页自己完成全部 API 交互。

## 12.3 gen_sign 反编译结果（sync_sdk::security）

函数名经 Ghidra 还原：
- `chksum_hash_sha2_256::digest::Digest::to_hex_lowercase` → **SHA-256 + 小写 hex**
- `chksum_hash_core::hash`
- `rand::rng::Rng::random_range`
- **`hmac::get_der_key` → HMAC**

因此 `sign = HMAC-SHA256(key, message)`，不是裸 sha256。

从 5 个 `_OBFBYTES_SDATA` 混淆块解出（逐字节，双重校验）：
```
k1 = "Fb93ZNftshbftT9143nbFtf8"      (26)
k2 = "zF01TfcmzRhmcs3qlzk4zh36"      (26)
"nonce=" (6)  "timestamp=" (10)  "sign=" (5)
```

格式化串（运行时解码，解析后）：
```
第一段： "{}_{}_{}_{}_{}"        → 5 槽 '_' 连接，构成 HMAC 输入
第二段： "{}&{}&{}"             → 拼出 nonce=..&timestamp=..&sign=..
```
第一段实参（按栈槽位精确推断）：
```
1. k1
2. 调用方传入的 &str  (param_4/param_5)
3. nonce   (i32, random_range 10000..100000000 → 与我们抓到的 8120301 吻合)
4. timestamp (u64 毫秒, now*1000)
5. hex(sha256(param_2, param_3))   ← chksum_hash_core::hash + to_hex_lowercase
```

对照 `NasConnOps::gen_authx`（另一变体）：6 槽
`PREFIX, param4, nonce, ts, digest, API_KEY`，nonce 范围 100000..1000000。
`gen_authx_header`（遥测用）：`md5` + `/api/v1/fn/con` + PREFIX/API_KEY。

## 12.4 未解点：槽 2 与槽 5 的取值

已穷举 **2200 万组**双样本联合校验（两个不同 nonce/timestamp 的真实抓包），
公式必须同时匹配两次，覆盖：5~6 槽排列、`_`/`&`/空 分隔、sha256/HMAC(4 把密钥)、
含 path/action/host/connid/trace/base64/hex 派生值 —— **全部未命中**。

**决定性实验**：把官方客户端产生的真实 `x-sign` 原样重放给真 NAS
（`192.168.1.100:5667`），返回 **`invalid sign`**。

> 结论：签名绑定了**目标主机或连接会话状态**（客户端对 `.88` 计算的签名，
> 拿去 `.168` 用不了）。因此槽 2 / 槽 5 必然包含 host、connid、trace-id
> 或其组合，且不是简单的单值拼接。

## 12.5 闭环所需的下一步（二选一）

**A. 让 Windows 客户端经我们的代理连真 NAS**
   - 在 Windows 里把代理指向本机 5667→真 NAS 的转发端口
   - 抓到的 sign 对真 NAS 有效 → 可拿它反推（此时 host 已知为真 NAS）
   - 并可用重放验证候选公式

**B. winedbg 动态取消息体（最直接）**
   - Wine 跑 exe，在 `hmac` update 处下断点，dump 消息缓冲区
   - 一次性拿到完整输入字符串，无需再猜

## 12.6 已可交付的部分

- TLS 自签证书处理（窗口级 `TLSErrorsPolicy::Ignore`）✅ 实测通过
- 完整请求头构造（含 base64 编码、connid、trace-id）✅
- 真实端点与错误语义（`/sync/comm` 校验链、502=服务未启动、`oauthapi/*`）✅
- SQLite schema / 任务管理 / inotify / 过滤 / 冲突处理 ✅ 37 单测通过

---

# 13. 签名算法破解完成（真机验证通过）

## 13.1 最终公式（100% 确认）

```text
digest = hex(sha256(x_comm_action))                 # 小写 hex, 64 字符
msg    = "{K1}_{path}_{nonce}_{timestamp}_{digest}"
sign   = HMAC-SHA256(key = K2, msg)                 # 小写 hex
x-sign = "nonce={nonce}&timestamp={timestamp}&sign={sign}"
```

常量（26 字节，从 `_OBFBYTES_SDATA` 逐字节解码）：
```
K1 = "Fb93ZNftshbftT9143nbFtf8Md"
K2 = "zF01TfcmzRhmcs3qlzk4zh36nC"
```
`path` = 请求路径（如 `/sync/comm`），`nonce` ∈ [10000, 100000000)，`timestamp` = 毫秒。

**验证**：3 个独立真实抓包样本全部精确匹配；用自算签名请求真 NAS 返回
`Mainland`（NAS 版本信息），而非 `invalid sign`。已写入 Rust 单元测试。

> 踩坑记录：早期解码漏掉尾部 2 字节（长度字段是 26，只解出 24），
> 导致 2200 万组穷举全部失败。

## 13.2 请求头（抓包确认）

```
x-watch-root: <base64(path)>      x-file: <base64(path)>
x-connid: <u64 随机，约 1e18 量级>
x-trace-id: <递增序号>             x-ops-hash: 0
x-comm-action: <动作名>            x-sign: <见上>
```
服务端**有 nonce 重放保护**（重复 nonce → `duplicat nonce`）。

## 13.3 WebSocket 通道（已打通升级）

```
GET /sync/event/register
  Upgrade: websocket / Connection: Upgrade
  x-session-id: <32 hex>
  x-sign: <同上算法，path=/sync/event/register>
  Cookie: mode=relay

→ HTTP/1.1 101 Switching Protocols
  x-server-version: 0.20.12
  x-server-settings: CAEQAA==   (protobuf: schema_version=1, modify_missing_as_create_default=0)
  x-ws-wire-format: json-v1
```

**升级成功但连接随即关闭** —— 说明还需登录态（`x-user-token`）。
试过 7 种 HandShakeReq JSON 形态，服务端均直接断连。

## 13.4 两平台 OAuth 路径不同（关键）

| 平台 | OAuth 实现 |
|---|---|
| macOS (0.2.3 dmg) | 直接打开 `https://{nas}/c/oauth`；scheme `fnsync://` (Info.plist) |
| **Windows (0.2.3 exe)** | **本地 axum 代理** + `http://127.0.0.1:{port}/async-oauth/c/...` |

Windows 版日志串（exe 内）：
```
oauth proxy started on port: {port}
axum proxy listening on http://...
open_oauth_window: {..}, {..}, domain: {..}
oauth window closed async-oauth
oauth proxy state cleaned:
```
即：webview 的 `proxy_url` 指向本地代理，代理转发到 NAS 并捕获授权回传。
exe 内**不含 `/c/oauth` 字面量**。

## 13.5 NAS 端点现状（真机 192.168.1.100:5667）

| 路径 | 结果 | 说明 |
|---|---|---|
| `/sync/comm` | 400 → 加签名后 200 `Mainland` | 同步服务正常 |
| `/sync/event/register` | 101 | 需要登录态才保持 |
| `/trimcon` | 200 空 | 同步服务探测端点 |
| `/fn-sync/` | 200 | 同步 App 的 Web UI（下载引导页） |
| `/license/v1/device/baseInfo` | 200 | 免鉴权 |
| `/cgi/*` | `invalid token` | 受保护网关 |
| `/v1/*` | 502 | 上游服务未运行 |
| `/c/oauth` `/async-oauth/c/*` | 404 | 不存在 |

## 13.6 剩余最后一环：登录 → 拿 token

需要 `x-user-token` 才能让 WS 保持。获取途径：
1. NAS Web 登录后的 `localStorage['fnos-token']`
2. 官方客户端完成 OAuth 后下发

验证方法：拿 token 作为 `x-user-token` 连 `/sync/event/register`，
若连接保持并可收发帧即成功。

---

# 14. 完整 API 协议（已实现并实测通过）

## 14.1 主 RPC 通道

```
WS  wss://<host>:5667/websocket?type=main
Header: cookie: mode=relay
```
每帧一个文本消息。**认证后所有请求都必须签名**：
```
消息体 = base64(HMAC-SHA256(key, json)) + json
```
（注意是字符串直接拼接，无分隔符）

## 14.2 登录（RSA + AES 混合加密，实测可用）

```
1) {"req":"util.crypto.getRSAPub"}     → {"pub":"-----BEGIN PUBLIC KEY-----..."}
2) {"req":"util.getSI"}                → {"si":"<18位数字>"}

3) 客户端生成：
     key_str = 32 个随机字符 [0-9a-zA-Z]      ← AES-256 密钥（ASCII）
     iv      = 16 字节随机
   aes = base64( AES-256-CBC( json(payload), key=key_str, iv=iv ) )   # PKCS#7
   rsa = base64( RSA-PKCS1v15(pub, key_str) )                        # 加密的是字符串本身

4) 发送 {"req":"encrypted","iv":b64(iv),"rsa":rsa,"aes":aes}

5) payload(json) =
   {"req":"user.login","user":<用户名>,"password":<密码>,
    "deviceName":"PcSync","deviceType":"PC","stay":true,
    "did":"<设备id>","ver":2,"si":<si>}

6) 响应 =
   {"uid":1000,"admin":true,"secret":"<b64>","ticket":"<b64>",
    "backId":..,"machineId":..,"result":"succ"}

   ★ secret 是用 key_str/iv **AES 加密**的：
     真实 HMAC 密钥 = AES-256-CBC-decrypt(b64decode(secret), key_str, iv)  # 16 字节
```

## 14.3 认证后请求（实测）

签名密钥 = 上面解出的 16 字节。示例：
```python
sig = base64(hmac_sha256(key, json_body))
frame = sig + json_body
```

可用 RPC：
| req | 说明 |
|---|---|
| `file.lsDir` | 存储卷列表 `[{name,v,uid,mtim,btim,dir}]` |
| `file.ls {"path":"vol2/1000/下载"}` | 目录内容 `[{name,uid,size,mtim,btim}]` |
| `file.mkdir {"path":...}` | 建目录 |
| `appcgi.sysinfo.getTrimVersion` | `{"trimVersion":"1.2.1010","edition":"Mainland-PE","platform":"x86"}` |
| `appcgi.sysinfo.getHostName` | `{"hostName":"nas","hasUsers":true,"trimVersion":"1.2.1010"}` |
| `user.info` | `{"userInfo":{"user":"xmlijian","uid":1000,"admin":true,"groups":[...]}}` |
| `stor.getUserStorage` | `{"stor":[{id,level:"basic",fstype:"btrfs",diskType:"SSD",fssize,frsize}]}` |

**实测结果**：登录成功，读出 12 个卷（HDD / 下载 / 零壹桥V10 / iso / 通联项目 /
Docker / 百度网盘 / vscode-workspace / …），存储 btrfs/SSD。

## 14.4 参考实现

`re-work/nas_rpc.py` —— 完整可运行客户端（登录 + 签名 + 文件 API）。

## 14.5 上传 / 下载（实测可用）

### 下载
```
RPC  file.download {files:["vol2/1000/下载/x.txt"]}
  → {"download":[{"uri":"/download/vol2/1000/%E4%B8%8B%E8%BD%BD/x.txt?t=1795766636&sign=XXX="}]}
GET https://<host>:5667<uri>      # sign 本身即授权，无需 cookie
```
注意：`file.download` 的响应可能分两帧（先 `{"result":"succ","uver":...}` 再 `{"download":[...]}`），
客户端需持续读直到出现 `download` 字段。

### 上传（三步）
```
1) RPC  file.checkUpload {"size":<n>,"path":"vol2/1000/下载/x.txt","overwrite":true}
   → {"uploadName":"x.txt.~#0","result":"succ"}
   ★ 服务端分配的临时名，必须用它作为上传目标

2) POST https://<host>:5667/upload
   Trim-Path:      encodeURI("vol2/1000/下载/x.txt.~#0")   # JS encodeURI 规则，保留 '/'
   Trim-From:      0
   Trim-Overwrite: true|false
   Trim-Mtim:      <unix 秒>
   Trim-Token:     <user.login 返回的 token>
   Trim-Sign:      base64(HMAC-SHA256(key, encodeURI(目标路径)))
   Content-Type:   multipart/form-data
   字段: trim-upload-file = <文件内容>
   （可选 Authorization: trim <token>）
   → 200, 空 body

3) 服务端自动把 x.txt.~#0 重命名为最终名 x.txt（无需额外 RPC）
```

## 14.6 Rust 实现与验证

`src-tauri/src/connect/{crypto.rs, nas_client.rs}` —— 完整 Rust 实现。

自检命令（对真实 NAS 执行）：
```
fn-sync-client --selftest-nas  <host[:port]> <user> <password>
   → 登录 → 读卷列表 → 上传 → 下载 → 内容校验 → 清理

fn-sync-client --selftest-sync <host[:port]> <user> <password>
   → 建 NAS 目录 → 启动真实同步 actor（inotify + 10s 远端轮询）
   → 本地写文件验证上传 → NAS 写文件验证下载 → 清理
```

实测结果（NAS 192.168.1.100, fnOS 0.20.12 / trim 1.2.1010）：
```
[1/5] 登录成功 uid=1000
[2/5] 卷: ["HDD","下载","零壹桥V10","iso","通联项目","Docker","百度网盘",
           "ISO","vscode-workspace","vscode-tools","preseed","ubuntu-seed"]
[3/5] 上传 58 字节
[4/5] 下载回读
[5/5] 内容一致
SELFTEST_OK

SELFTEST_SYNC_OK 双向同步验证通过
```

## 14.7 已废弃的路径（保留备查）

早期误判的 `/c/oauth` OAuth 授权流程 **不属于 NAS**：
- `/c/oauth` 只存在于**客户端本地启动的 axum 代理**（`oauth proxy started on port:`）
- NAS 侧 `/c/<短串>` 是 nginx 短链规则（至少 6 字符）
- 正确做法：直接在主 RPC 通道走 `user.login`（见 §14.2）

`/sync/event/register` 的 protobuf 事件通道**未采用**：
官方同步客户端确实用它（`x-sign` 为 136 字节 RSA 密文块，
`x-ws-wire-format: protobuf`），但 Linux 版改用
「inotify + 10 秒远端目录轮询」实现等价同步，已实测双向可用。
