# 飞牛同步 Linux 客户端（非官方）— 代码仓库

> **非官方声明**：本项目为非官方的 Linux 客户端，`official-ui/` 前端产物与图标等资源
> 来源于飞牛官方安装包，仅用于个人使用与兼容性研究，与飞牛官方无直接关联，
> 未经官方授权。如有侵权，请联系删除。

逆向自官方 fn-sync 0.2.3（Windows/macOS）的 Linux 客户端，同一份业务代码
（NAS 连接、同步引擎、按需同步 FUSE、日志/设置界面）跑在两个框架变体上。

## 两个交付版本

| 目录 | 版本 | 运行框架 | 目标系统 | 什么时候用 |
|---|---|---|---|---|
| `fn-sync-linux/` | **新系统版** | Tauri 2 + webkit2gtk-**4.1** | Ubuntu 22.04+/24.04+、Debian 12+、Fedora 38+ 等 | 系统自带 `libwebkit2gtk-4.1` 且 glibc 较新 |
| `fn-sync-linux-webkit40/` | **信创版** | Tauri 1 + webkit2gtk-**4.0** | 麒麟 V10 SP1、UOS 20 专业版（x86_64 / aarch64） | 国产化系统只有 4.0 运行库 |

选型原则：**新系统一律用 Tauri 2 版**；国产化系统（麒麟/UOS）用 Tauri 1 版
（它们的软件源里没有 webkit2gtk-4.1，也不建议自建 WebKit，详见
`fn-sync-linux-webkit40/README.md` 的说明）。

各版本的详细设计、构建依赖与验证记录见各自目录下的 `README.md`。

## 两份代码的关系

业务代码同源，差异只在框架胶水和平台集成：

- 共享：`src-tauri/src/connect|service|tasks|platform` 的业务逻辑、协议文档、前端 `official-ui`；
- 信创版额外有：Peony/DDE 文件管理器插件（`plugins/`）、麒麟/Qt 版本兼容构建脚本、
  按需同步空目录校验、崩溃后死挂载自动回收；
- 新系统版额外有：Tauri 2 专属胶水（插件初始化、窗口 API）、Nautilus 角标扩展。

**修改约定**：业务修复先改一边，再把差异回灌另一边，避免两份漂移。
当前应保持同步的点：

1. `src-tauri/src/service/tasks.rs` — 按需同步空目录校验；
2. `src-tauri/src/tasks/ondemand/mod.rs` — 死挂载回收（fusermount3→fusermount）、
   多实例保护、挂载前空目录兜底；
3. `official-ui` CSS 字体栈与字号（跟随系统字体）。

回灌方式（两个版本在同一仓库内）：

```bash
# 查看信创版某文件最近的改动
cd fn-sync-linux-webkit40
git log -p -- src-tauri/src/tasks/ondemand/mod.rs
# 手工应用到 fn-sync-linux/ 对应文件后跑测试
```

## 构建与产物

各目录内自带构建脚本，顶层不重复：

```bash
# 新系统版（amd64，本机需 webkit2gtk-4.1 开发包）
cd fn-sync-linux && ./debian-build.sh
# 产物：src-tauri/target/debian/fn-sync-client_0.2.3-20_amd64.deb（DEB_REVISION 可改）
# AppImage：./scripts/build-appimage.sh → artifacts/fn-sync-client-0.2.3-x86_64.AppImage
# 交付目录：artifacts/新系统版/（deb + AppImage + SHA256SUMS.txt）

# 信创版（常驻容器 webkit40-build / webkit40-arm，glibc 2.28）
cd fn-sync-linux-webkit40
./build-docker.sh          # x86_64 deb（含 DDE 插件）
./build-docker-arm64.sh    # aarch64 deb（含 Peony 插件）
./scripts/build-appimage.sh
./scripts/package-xinchuang.sh
# 交付目录：artifacts/信创版/
```

版本发布流程与资产命名见 [`docs/PUBLISHING.md`](docs/PUBLISHING.md)。

## 仓库结构

本仓库为 monorepo：

- `fn-sync-linux/` — 新系统版（Tauri 2），保留原有提交历史（`git filter-repo` 移入子目录）；
- `fn-sync-linux-webkit40/` — 信创版（Tauri 1），以子树方式并入（2026-11-26）。

构建产物、`target/`、`node_modules/`、`artifacts/` 中的二进制不进 git。

本地工作资料（逆向抓包、兼容性调研、官方安装包等）不纳入本仓库。
