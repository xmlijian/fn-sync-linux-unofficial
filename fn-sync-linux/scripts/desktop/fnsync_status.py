"""Nautilus 扩展：按需同步状态列 + 右键菜单。

对齐官方客户端的动作集合：
  Windows Explorer 菜单          macOS Finder 动作
  download                  →    （双击/读取即下载）
  free_space                →    释放空间
  always_stay_local         →    始终保留在此 Mac
  cancel_stay_local         →    取消始终保留

安装：~/.local/share/nautilus-python/extensions/fnsync_status.py
生效：nautilus -q
"""

import os
import sys
import subprocess
import threading
import weakref
from urllib.parse import unquote, urlparse
import gi
try:
    gi.require_version("Nautilus", "4.1")
except ValueError:
    gi.require_version("Nautilus", "3.0")
from gi.repository import GObject, Nautilus, GLib
sys.path.insert(0, "/usr/lib/fn-sync-client")
from fnsync_ipc import snapshots

_CACHE = {}
_LOCK = threading.Lock()

ATTR_STATE = "user.fnsync.state"
COLUMN_ATTR = "fnsync_status"
CTL = "/usr/bin/fnsync-ctl"

LABEL = {
    "queued": "等待下载",
    "failed": "操作失败",
    "cached": "已下载",
    "placeholder": "未下载",
    "dirty": "本地修改",
    "download": "下载中…",
    "upload": "上传中…",
}


def _path_of(file_info):
    uri = file_info.get_uri()
    if not uri or not uri.startswith("file://"):
        return None
    return unquote(urlparse(uri).path)


def _xattr(path, name):
    with _LOCK:
        state = _CACHE.get(path)
    if not state:
        return None
    if name == ATTR_STATE:
        return "failed" if state.get("error") else state["state"]
    if name == "user.fnsync.progress":
        return str(state["progress"])
    if name == "user.fnsync.pinned":
        return "1" if state["pinned"] else "0"
    return None


def _state_of(path):
    return _xattr(path, ATTR_STATE)


def _label(path):
    state = _state_of(path)
    if state is None or state == "directory":
        return ""
    if state in ("download", "upload"):
        return f"{LABEL.get(state, "状态未知")} {_xattr(path, 'user.fnsync.progress')}%"
    return LABEL.get(state, "状态未知")


def _run(args):
    try:
        subprocess.Popen(
            [CTL] + args,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
    except Exception:
        pass


class FnSyncStatusExtension(GObject.GObject, Nautilus.InfoProvider, Nautilus.ColumnProvider, Nautilus.MenuProvider):
    def __init__(self):
        super().__init__()
        self._files = weakref.WeakValueDictionary()
        self._busy = False
        GLib.timeout_add(500, self._refresh)

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
            changed = {p for p in set(_CACHE) | set(new) if _CACHE.get(p) != new.get(p)}
            _CACHE = new
        self._busy = False
        for path in changed:
            info = self._files.get(path)
            if info:
                info.invalidate_extension_info()
        return False

    # ---------- 状态列 ----------
    def update_file_info(self, file_info):
        path = _path_of(file_info)
        if path:
            self._files[path] = file_info
        file_info.add_string_attribute(COLUMN_ATTR, _label(path) if path else "")
        return Nautilus.OperationResult.COMPLETE

    def get_columns(self):
        return (
            Nautilus.Column(
                name="FnSyncStatusExtension::sync_status",
                attribute=COLUMN_ATTR,
                label="同步状态",
                description="按需同步：已下载 / 未下载 / 下载中 / 本地修改",
            ),
        )

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

        states = [_state_of(p) for p in paths]
        if all(s is None for s in states):
            return []  # 不在按需同步目录内

        pinned = [(_xattr(p, "user.fnsync.pinned") == "1") for p in paths]

        # 不折叠：每个动作都是一级独立菜单项，方便一眼看到、少点一次。
        # nautilus-python 4.x 已移除分隔符 API（GNOME 43 重写扩展接口时删掉），
        # 无法在两个横线之间自成一组，因此统一加「飞牛同步」前缀做视觉归组。
        items = []
        placeholders = [p for p, s in zip(paths, states) if s == "placeholder"]
        if placeholders:
            it = self._make("FnSync::download", "飞牛同步：下载到本地")
            it.connect("activate", lambda _i: _run(["download"] + placeholders))
            items.append(it)

        releasable = [
            p for p, s, pin in zip(paths, states, pinned)
            if s == "cached" and not pin
        ]
        if releasable:
            it = self._make("FnSync::free", "飞牛同步：释放空间（只删本地副本）")
            it.connect("activate", lambda _i: _run(["free"] + releasable))
            items.append(it)

        if any(not pin for p, pin in zip(paths, pinned) if _state_of(p)):
            it = self._make("FnSync::pin", "飞牛同步：始终保留在此电脑")
            it.connect("activate", lambda _i: _run(["pin"] + paths))
            items.append(it)

        if any(pinned):
            it = self._make("FnSync::unpin", "飞牛同步：取消始终保留")
            it.connect("activate", lambda _i: _run(["unpin"] + paths))
            items.append(it)

        return items

    def get_background_items(self, *args):
        return []
