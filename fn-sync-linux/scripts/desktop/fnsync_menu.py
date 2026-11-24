"""按需同步右键菜单规格（纯逻辑，不依赖 GI/Nautilus）。

fnsync_status.py 把文件管理器条目转成 entries 后调用 menu_specs()，
再包装成 Nautilus.MenuItem；test_fnsync_menu.py 直接测试这里，
不需要启动文件管理器。

entry = {"path": str, "state": str | None, "pinned": bool}
spec  = {"key": str, "label": str, "action": str, "targets": [str, ...]}

state 使用守护进程的底层状态（不含错误覆盖），因此一次操作失败不会
让菜单入口消失。
"""

PREFIX = "飞牛同步："


def _unique(entries):
    seen = set()
    out = []
    for entry in entries:
        path = entry.get("path")
        if not path or path in seen:
            continue
        seen.add(path)
        out.append(entry)
    return out


def _spec(key, label, action, targets):
    return {
        "key": key,
        "label": label,
        "action": action,
        "targets": list(targets),
    }


def menu_specs(entries):
    """根据选中项生成菜单规格；空列表表示不显示飞牛同步菜单。"""
    entries = [e for e in _unique(entries) if e.get("state") is not None]
    if not entries:
        return []

    dirs = [e["path"] for e in entries if e["state"] == "directory"]
    files = [e for e in entries if e["state"] != "directory"]

    specs = []

    # 下载：未下载的文件 / 整个文件夹
    download = [e["path"] for e in files if e["state"] == "placeholder"]
    if download:
        specs.append(_spec("download", PREFIX + "下载到本地", "download", download))
    if dirs:
        specs.append(_spec("download_dir", PREFIX + "下载此文件夹", "download", dirs))

    # 释放：已下载且未保留的文件 / 整个文件夹
    free = [
        e["path"] for e in files
        if e["state"] == "cached" and not e.get("pinned")
    ]
    if free:
        specs.append(_spec("free", PREFIX + "释放空间（只删本地副本）", "free", free))
    if dirs:
        specs.append(_spec("free_dir", PREFIX + "释放此文件夹（只删本地副本）", "free", dirs))

    # 保留：文件是单文件 pin；文件夹是“保留并下载当前已有文件”的一次性动作
    pin = [e["path"] for e in files if not e.get("pinned")]
    if pin:
        specs.append(_spec("pin", PREFIX + "始终保留在此电脑", "pin", pin))
    if dirs:
        specs.append(
            _spec("pin_dir", PREFIX + "保留并下载此文件夹（当前文件）", "pin", dirs)
        )

    # 取消保留：文件只对已保留项显示；文件夹始终提供批量取消入口
    unpin = [e["path"] for e in files if e.get("pinned")]
    if unpin:
        specs.append(_spec("unpin", PREFIX + "取消始终保留", "unpin", unpin))
    if dirs:
        specs.append(
            _spec("unpin_dir", PREFIX + "取消文件夹内所有保留", "unpin", dirs)
        )

    return specs
