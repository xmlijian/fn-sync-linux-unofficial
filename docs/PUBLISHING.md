# 发布流程

## 仓库定位

非官方 Linux 客户端，包含新系统版（Tauri 2 / webkit2gtk-4.1）与信创版
（Tauri 1 / webkit2gtk-4.0）两个交付版本。当前仓库为 **私有**，公开发布前
先完成文末的检查清单。

## 版本与 tag

- tag 统一使用 `vX.Y.Z`（与上游官方客户端版本号对齐，当前 `v0.2.3`）；
- 一个 Release 挂全部资产，用户按系统选择：

| 资产 | 对应版本 | 系统 |
|---|---|---|
| `fn-sync-client_X.Y.Z_amd64.deb` | 新系统版 | Ubuntu 22.04+/Debian 12+（amd64） |
| `fn-sync-client_X.Y.Z_x86_64.AppImage` | 新系统版 | 通用 x86_64 |
| `fn-sync-client_X.Y.Z_xinchuang_amd64.deb` | 信创版 | 麒麟 V10 / UOS（x86_64） |
| `fn-sync-client_X.Y.Z_xinchuang_arm64.deb` | 信创版 | 麒麟 V10 / UOS（aarch64） |
| `SHA256SUMS.txt` | 全部 | 校验文件 |

## 发布步骤

1. 在对应目录构建：新系统版 `cd fn-sync-linux && ./debian-build.sh && ./scripts/build-appimage.sh`；
   信创版 `cd fn-sync-linux-webkit40 && ./build-docker.sh && ./build-docker-arm64.sh && ./scripts/package-xinchuang.sh`。
2. 在交付目录生成校验和：`sha256sum * > SHA256SUMS.txt`。
3. 打 tag 并推送：`git tag -a v0.2.3 -m "v0.2.3" && git push origin v0.2.3`。
4. 创建 Release：

   ```bash
   gh release create v0.2.3 \
     --title "v0.2.3" \
     --notes-file docs/release-notes/v0.2.3.md \
     <各交付产物路径...>
   ```

5. 发布后在 README 中更新下载指引。

> 私有仓库的 Release 仅协作者可见；GitHub Actions 在私有仓库有分钟数限制，
> 信创版 docker 构建耗时较长，第一版采用手动上传。

## 日常同步约定

两个版本在同一仓库内，业务修复先改一边，再手工回灌另一边，提交时同时说明
影响范围。重点同步文件见根 README 的「两份代码的关系」。

## 私有 → 公开检查清单

- [ ] 根 README 顶部保留非官方声明；
- [ ] 补充 LICENSE（当前无）；
- [ ] 确认 `official-ui/`、`icons/`、`protocol-docs/` 的保留决定（当前决定保留，
      来源为官方安装包）；
- [ ] 确认 Release 资产中不含个人 NAS 地址、账号、token 等调试残留；
- [ ] 清理 `.omc/`、`.omx/`、日志、抓包结果等本地工具产物（已在 `.gitignore` 中忽略，
      公开前用 `git ls-files` 复查历史提交中无残留）。
