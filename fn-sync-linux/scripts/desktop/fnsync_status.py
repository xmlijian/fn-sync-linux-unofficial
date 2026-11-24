"""Nautilus 扩展：按需同步状态图标（emblem）+ 右键菜单。

状态用文件图标旁的小角标表示（列表/图标视图都可见），不占用文字列，
也不会随进度刷新造成界面抖动。

对齐官方客户端的动作集合：
  Windows Explorer 菜单          macOS Finder 动作
  download                  →    （双击/读取即下载）
  free_space                →    释放空间
  always_stay_local         →    始终保留在此 Mac
  cancel_stay_local         →    取消始终保留

文件和文件夹都提供下载 / 释放 / 保留 / 取消保留；文件夹级操作由
fnsync-ctl 递归展开。文件夹的“保留”是一次性动作：只对点击时已有的
文件生效，之后新增的文件不会被自动保留。

安装：/usr/share/nautilus-python/extensions/fnsync_status.py
生效：nautilus -q
"""

import os
import sys
import subprocess
import threading
from urllib.parse import unquote, urlparse
import gi
try:
    gi.require_version("Nautilus", "4.1")
except ValueError:
    gi.require_version("Nautilus", "3.0")
from gi.repository import GObject, Nautilus, GLib
sys.path.insert(0, "/usr/lib/fn-sync-client")
from fnsync_ipc import snapshots
from fnsync_menu import menu_specs

_CACHE = {}
_LOCK = threading.Lock()

CTL = "/usr/bin/fnsync-ctl"

# 状态 → 文件图标角标（彩色，实心/空心对比明显）
# 失败不单独做角标：显示文件真实状态（未下载/已下载），失败由弹窗提示。
EMBLEMS = {
    "placeholder": "emblem-fnsync-online",    # 灰色空心云：仅在线
    "cached": "emblem-fnsync-local",          # 绿色实心云+白勾：已下载
    "download": "emblem-fnsync-downloading",  # 蓝色实心云+白下箭头
    "upload": "emblem-fnsync-uploading",      # 蓝色实心云+白上箭头
    "queued": "emblem-fnsync-downloading",    # 排队等同下载中，不单独用时钟
    "releasing": "emblem-fnsync-online",      # 释放中共用灰色云
    "dirty": "emblem-fnsync-modified",        # 橙色铅笔：本地修改
}


def _notify(title, body):
    try:
        subprocess.Popen(
            ["notify-send", "-a", "飞牛同步", title, body],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    except OSError:
        pass


def _display_key(state):
    """角标只随底层状态变化，不随进度/错误/保留变化 —— 避免频繁 invalidate
    文件管理器条目导致界面抖动。"""
    return state.get("state") if state else None


def _path_of(file_info):
    uri = file_info.get_uri()
    if not uri or not uri.startswith("file://"):
        return None
    return unquote(urlparse(uri).path)


def _pinned(path):
    with _LOCK:
        state = _CACHE.get(path)
    return bool(state and state.get("pinned"))


def _snapshot_of(path):
    with _LOCK:
        return _CACHE.get(path)


def _base_state(path):
    """底层状态（不含错误覆盖）：右键菜单必须按它判断，否则一次失败后
    文件既不是 placeholder 也不是 cached，所有操作入口都会消失。"""
    snap = _snapshot_of(path)
    return snap.get("state") if snap else None


def _run(args):
    try:
        env = dict(os.environ, FNSYNC_GUI="1")
        subprocess.Popen(
            [CTL] + args,
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
    except Exception:
        pass


class FnSyncStatusExtension(GObject.GObject, Nautilus.InfoProvider, Nautilus.MenuProvider):
    def __init__(self):
        super().__init__()
        # 必须强引用 file_info：弱引用可能在 Nautilus 释放 wrapper 后失效，
        # 导致状态变化后角标不再刷新。
        self._files = {}
        self._busy = False
        GLib.timeout_add(1000, self._refresh)

    def _refresh(self):
        if self._busy:
            return True
        self._busy = True
        def read():
            new = {}
            for _sock, status in snapshots():
                for state in status["files"]:
                    new[os.path.join(status["mountpoint"], state["path"])] = state
            GLib.idle_add(self._apply, new)
        threading.Thread(target=read, daemon=True).start()
        return True

    def _apply(self, new):
        global _CACHE
        with _LOCK:
            old = _CACHE
            # 只有“显示相关字段”变化才 invalidate；进度每 500ms 变化不再触发刷新。
            changed = {
                p for p in set(old) | set(new)
                if _display_key(old.get(p)) != _display_key(new.get(p))
            }
            _CACHE = new
        self._busy = False
        # 上传完成/失败提示：文件从“上传中”离开时通知（下载由 fnsync-ctl 进度窗负责）。
        for path in changed:
            before = old.get(path)
            after = new.get(path)
            if (
                not before
                or not after
                or before.get("state") != "upload"
                or after.get("state") == "upload"
            ):
                continue
            name = os.path.basename(path)
            if after.get("error"):
                _notify("上传失败", f'{name}\n{after["error"]}')
            else:
                _notify("上传完成", name)
        # 强引用只保留当前仍存在的文件，避免长期累积。
        for path in list(self._files):
            if path not in new:
                self._files.pop(path, None)
        for path in changed:
            info = self._files.get(path)
            if info:
                info.invalidate_extension_info()
        return False

    # ---------- 状态角标 ----------
    def update_file_info(self, file_info):
        path = _path_of(file_info)
        if not path:
            return Nautilus.OperationResult.COMPLETE
        self._files[path] = file_info
        emblem = EMBLEMS.get(_base_state(path))
        if emblem:
            file_info.add_emblem(emblem)
        return Nautilus.OperationResult.COMPLETE

    # ---------- 右键菜单 ----------
    def _make(self, name, label):
        item = Nautilus.MenuItem(name=name, label=label)
        return item

    def get_file_items(self, *args):
        files = args[-1] if args else []
        if not files:
            return []

        paths = [_path_of(f) for f in files]
        paths = [p for p in paths if p]
        if not paths:
            return []

        # 菜单操作按底层状态判断；操作失败只弹提示，不能把入口藏掉。
        # 目录状态为 "directory"，由 fnsync_menu 决定显示哪些文件夹级动作。
        entries = [
            {"path": p, "state": _base_state(p), "pinned": _pinned(p)}
            for p in paths
        ]
        specs = menu_specs(entries)

        # 不折叠：每个动作都是一级独立菜单项，方便一眼看到、少点一次。
        # nautilus-python 4.x 已移除分隔符 API（GNOME 43 重写扩展接口时删掉），
        # 无法在两个横线之间自成一组，因此统一加「飞牛同步」前缀做视觉归组。
        items = []
        for spec in specs:
            it = self._make("FnSync::" + spec["key"], spec["label"])
            it.connect(
                "activate",
                lambda _i, s=spec: _run([s["action"]] + s["targets"]),
            )
            items.append(it)
        return items

    def get_background_items(self, *args):
        return []
