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
import time
from urllib.parse import unquote, urlparse
import gi
try:
    gi.require_version("Nautilus", "4.1")
except ValueError:
    gi.require_version("Nautilus", "3.0")
from gi.repository import GObject, Nautilus, GLib
sys.path.insert(0, "/usr/lib/fn-sync-client")
from fnsync_ipc import snapshots, sockets, watch
from fnsync_menu import menu_specs

_CACHE = {}
_LOCK = threading.Lock()

# 快照短暂缺文件时保留旧状态的时间，避免图标整屏闪一下。
STATE_GRACE_SECS = 30.0
# 守护进程完全无响应（例如重启）时，旧角标最多保留这么久。
EMPTY_GRACE_SECS = 300.0

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
        # 最后一次见到该路径的时间（monotonic）。守护进程重启或目录尚未
        # 加载时，快照会短暂缺少已显示文件；不能立刻丢掉强引用，否则重连
        # 后无法 invalidate，图标会停在旧状态（与右键菜单不一致）。
        self._last_seen = {}
        self._busy = False
        # 订阅守护进程的状态事件：下载/上传/释放/修改/pin 变化时立即刷新，
        # 不再只依赖定时轮询。300ms 轮询保留作为兜底（断线/事件丢失）。
        self._watchers = set()
        self._empty_since = None
        self._discover_watchers()
        GLib.timeout_add(2000, self._discover_watchers)
        GLib.timeout_add(300, self._refresh)

    def _discover_watchers(self):
        """为新出现的按需同步挂载点启动订阅线程。"""
        for sock in sockets():
            if sock in self._watchers:
                continue
            self._watchers.add(sock)
            threading.Thread(
                target=self._watch_socket, args=(sock,), daemon=True
            ).start()
        return True

    def _watch_socket(self, sock):
        def on_event():
            # watch() 在后台线程回调；切回 GLib 主线程执行刷新。
            GLib.idle_add(self._refresh_once)

        try:
            watch(sock, on_event)
        except Exception:
            pass

    def _refresh_once(self):
        # idle 回调：执行一次刷新后移除自身（_refresh 的返回值只适用于 timeout）。
        self._refresh()
        return False

    def _refresh(self):
        if self._busy:
            return True
        self._busy = True
        def read():
            # 无论成功失败都要复位 _busy：一次异常若把标志留在 True，
            # 后续所有刷新都会被跳过，角标从此不再更新。
            try:
                new = {}
                for _sock, status in snapshots():
                    for state in status["files"]:
                        new[os.path.join(status["mountpoint"], state["path"])] = state
            except Exception:
                GLib.idle_add(self._release_busy)
                return
            GLib.idle_add(self._apply, new)
        threading.Thread(target=read, daemon=True).start()
        return True

    def _release_busy(self):
        self._busy = False
        return False

    def _apply(self, new):
        global _CACHE
        # 一进入应用阶段就释放忙标志，任何异常都不会让后续刷新永久停摆。
        self._busy = False
        now = time.monotonic()
        skip = False
        force_clear = False
        with _LOCK:
            old = _CACHE
            if not new and old:
                if self._empty_since is None:
                    self._empty_since = now
                if now - self._empty_since < EMPTY_GRACE_SECS:
                    # 守护进程暂时不可用/重启中：保留旧状态，避免整屏角标消失
                    for p in old:
                        self._last_seen[p] = now
                    skip = True
                    merged = old
                    changed = set()
                else:
                    # 长时间无响应：确认守护进程不可用，清空
                    self._empty_since = None
                    force_clear = True
            if not skip:
                if force_clear:
                    merged = {}
                    changed = set(old)
                else:
                    self._empty_since = None
                    merged = dict(new)
                    # 快照短暂缺少文件（守护进程启动、目录尚未展开、单次状态
                    # 请求失败）时，短时间内保留旧状态，避免图标闪一下就不见。
                    for p, state in old.items():
                        if (
                            p not in merged
                            and now - self._last_seen.get(p, 0) <= STATE_GRACE_SECS
                        ):
                            merged[p] = state
                    # 只有“显示相关字段”变化才 invalidate；进度变化不触发刷新。
                    changed = {
                        p for p in set(old) | set(merged)
                        if _display_key(old.get(p)) != _display_key(merged.get(p))
                    }
            _CACHE = merged
        if skip:
            return False
        # 只在上传失败时提示；成功不打扰用户。
        for path in changed:
            before = old.get(path)
            after = new.get(path)
            if (
                not before
                or not after
                or before.get("state") != "upload"
                or after.get("state") == "upload"
                or not after.get("error")
            ):
                continue
            name = os.path.basename(path)
            _notify("上传失败", f'{name}\n{after["error"]}')
        for path in new:
            self._last_seen[path] = now
        # 超过 10 分钟仍未出现的条目才清理。守护进程重启、NAS 重连、目录
        # 尚未展开都会让快照短暂缺少文件；过早丢弃 file_info 会导致图标
        # 停在旧状态，而右键菜单实时读快照、看起来“不一致”。
        for path in list(self._files):
            if path not in new and now - self._last_seen.get(path, now) > 600:
                self._files.pop(path, None)
                self._last_seen.pop(path, None)
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
        self._last_seen[path] = time.monotonic()
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
