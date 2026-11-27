# 飞牛同步 Linux 客户端（非官方）

> **非官方声明**：本项目为非官方的 Linux 客户端，`official-ui/` 前端产物与图标等资源
> 来源于飞牛官方安装包，仅用于个人使用与兼容性研究，与飞牛官方无直接关联，
> 未经官方授权。如有侵权，请联系删除。

非官方的飞牛（fnOS）同步客户端 Linux 移植，逆向自官方 fn-sync 0.2.3（Windows/macOS）。
同一份业务代码（NAS 连接、同步引擎、按需同步 FUSE、日志与设置界面）跑在两个框架变体上，
面向较新的发行版与国产化信创系统分别提供安装包。

[下载](#下载) · [安装](#安装) · [已验证环境](#已验证环境) · [从源码构建](#从源码构建) · [许可证](#许可证)

## 特性

- **三种同步模式**：双向同步、仅上传、仅下载；
- **按需同步**：基于 FUSE 的占位文件与本地缓存，文件管理器角标显示状态；
- **会话自恢复**：NAS 侧 token 失效后自动用长期凭据重连；
- **文件管理器集成**：Nautilus（新系统版）、Peony / DDE（信创版）的角标与右键菜单；
- **托盘与状态面板**：传输进度、暂停/继续、日志查看；
- **多架构**：amd64 与 aarch64；信创版覆盖麒麟 V10 SP1、UOS 20 专业版。

## 两个交付版本

| 目录 | 版本 | 运行框架 | 目标系统 |
|---|---|---|---|
| `fn-sync-linux/` | 新系统版 | Tauri 2 + webkit2gtk-**4.1** | Ubuntu 22.04+/24.04+、Debian 12+、Fedora 38+ |
| `fn-sync-linux-webkit40/` | 信创版 | Tauri 1 + webkit2gtk-**4.0** | 麒麟 V10 SP1、UOS 20 专业版（x86_64 / aarch64） |

选型原则：新系统一律用 Tauri 2 版；国产化系统（麒麟/UOS）用 Tauri 1 版，
因为其软件源中没有 webkit2gtk-4.1。各版本的详细设计、构建依赖与验证记录见对应目录下的 `README.md`。

## 下载

预构建安装包见 [Releases](https://github.com/xmlijian/fn-sync-linux-unofficial/releases)，
附 `SHA256SUMS.txt` 校验文件。

| 版本 | 系统 | 文件 |
|---|---|---|
| 新系统版 | Ubuntu / Debian arm64 | `fn-sync-client_0.2.3-20_arm64.deb` |
| 新系统版 | Ubuntu / Debian amd64 | `fn-sync-client_0.2.3-20_amd64.deb` |
| 新系统版 | 通用 x86_64 | `fn-sync-client-0.2.3-x86_64.AppImage` |
| 新系统版 | 通用 aarch64 | `fn-sync-client-0.2.3-aarch64.AppImage` |
| 信创版 | 麒麟 V10 / UOS（x86_64） | `xinchuang_fn-sync-client_0.2.3_amd64.deb` |
| 信创版 | 麒麟 V10 / UOS（aarch64） | `xinchuang_fn-sync-client_0.2.3_arm64.deb` |
| 信创版 | x86_64 便携包 | `xinchuang_fn-sync-client-0.2.3-x86_64.AppImage` |
| 信创版 | aarch64 便携包 | `xinchuang_fn-sync-client-0.2.3-aarch64.AppImage` |

## 安装

```bash
# Debian / Ubuntu / 麒麟 / UOS
sudo dpkg -i <deb 文件>
sudo apt -f install

# AppImage
chmod +x <AppImage 文件>
./<AppImage 文件>
```

信创版 deb 内置 Peony（arm64）或 DDE（amd64）文件管理器插件；AppImage 不包含插件，
但同步、托盘与按需同步功能正常。

## 已验证环境

信创版（真机安装验证）：

| 系统 | CPU | 架构 | 验证内容 |
|---|---|---|---|
| 麒麟 V10 SP1 | 飞腾 D2000 | aarch64 | deb 安装、Peony 文件管理器集成、按需同步 |
| 麒麟 V10 SP1 | 麒麟 990 | aarch64 | deb 安装、wayland 会话显示、按需同步 |
| UOS 20 专业版 | 未记录 | x86_64 | deb 安装、DDE 文件管理器集成、按需同步 |

新系统版：Ubuntu 26.04 LTS（x86_64）完成 deb / AppImage 构建与运行验证；
aarch64 产物在 Ubuntu 22.04 arm64 环境中构建（glibc 符号上限 2.34），真机验证待补。

其它架构（如龙芯 loongarch64）尚未构建和验证。

## 从源码构建

各目录内自带构建脚本，顶层不重复：

```bash
# 新系统版（本机需 webkit2gtk-4.1 开发包）
cd fn-sync-linux && ./debian-build.sh
# AppImage
./scripts/build-appimage.sh

# 信创版（常驻容器 webkit40-build / webkit40-arm，glibc 2.28）
cd fn-sync-linux-webkit40
./build-docker.sh          # x86_64 deb（含 DDE 插件）
./build-docker-arm64.sh    # aarch64 deb（含 Peony 插件）
./scripts/build-appimage.sh
./scripts/package-xinchuang.sh
```

版本发布流程与资产命名见 [`docs/PUBLISHING.md`](docs/PUBLISHING.md)。

## 项目结构

本仓库为 monorepo：

- `fn-sync-linux/` — 新系统版（Tauri 2），保留原有提交历史；
- `fn-sync-linux-webkit40/` — 信创版（Tauri 1），以子树方式并入（2026-11-26）。

业务代码同源，差异只在框架胶水和平台集成。修改约定：业务修复先改一边，
再手工回灌另一边，避免两份漂移。当前需同步的关键文件：

1. `src-tauri/src/service/tasks.rs` — 按需同步空目录校验；
2. `src-tauri/src/tasks/ondemand/mod.rs` — 死挂载回收、多实例保护、挂载前空目录兜底；
3. `official-ui` CSS 字体栈与字号（跟随系统字体）。

构建产物、`target/`、`node_modules/`、`artifacts/` 中的二进制不进 git。

## 已知限制

- 按需同步依赖系统 FUSE 与 `fusermount3`；麒麟 134 等仅有 `fusermount` 的系统会自动回退；
- 协议为逆向所得，官方客户端更新后可能失效，需要重新抓包校准；
- 新系统版 aarch64 与龙芯等架构尚缺真机验证；
- 非官方项目，不保证与官方客户端的行为完全一致。

## 反馈

欢迎通过 [Issues](https://github.com/xmlijian/fn-sync-linux-unofficial/issues) 反馈问题，
请附上系统版本、架构、安装包类型和相关日志（注意隐去 NAS 地址与账号信息）。

## 许可证

本项目代码以 [GPL-3.0](LICENSE) 发布。`official-ui/`、`icons/` 等来源于官方安装包的
资源不在此许可证覆盖范围内，权利归原权利人所有。
