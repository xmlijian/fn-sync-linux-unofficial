"""Bounded local IPC client shared by CLI and file manager. Never accesses FUSE."""
import glob
import json
import os
import socket
import time


def request(sock, action, path="", timeout=3):
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(timeout)
        stream.connect(sock)
        stream.sendall((json.dumps({"action": action, "path": path}) + "\n").encode())
        data = bytearray()
        while b"\n" not in data:
            chunk = stream.recv(65536)
            if not chunk:
                raise RuntimeError("客户端连接已关闭")
            data.extend(chunk)
            if len(data) > 16 * 1024 * 1024:
                raise RuntimeError("状态响应过大")
    value = json.loads(data)
    if not value.get("ok"):
        raise RuntimeError(value.get("error", "操作失败"))
    return value["result"]


def sockets():
    """当前所有按需同步挂载点的控制 socket。"""
    return glob.glob(
        os.path.expanduser("~/.local/share/fn-sync-client/ondemand/*/control.sock")
    )


def snapshots():
    result = []
    for sock in sockets():
        try:
            result.append((sock, request(sock, "status")))
        except (OSError, ValueError, RuntimeError):
            continue
    return result


def watch(sock, on_event, stop=None):
    """阻塞订阅控制 socket 的状态事件；断线自动重连。

    服务端会先回一条 ack，之后每次状态变化推一行事件。每收到一行就调用
    on_event()（ack 也会触发一次，用于立即填充初始状态）。stop 是
    threading.Event 时用于测试/退出。
    """
    while stop is None or not stop.is_set():
        try:
            with socket.socket(socket.AF_UNIX) as stream:
                stream.connect(sock)
                stream.sendall(b'{"action":"subscribe"}\n')
                with stream.makefile("rb") as lines:
                    for _line in lines:
                        if stop is not None and stop.is_set():
                            return
                        on_event()
        except (OSError, ValueError):
            pass
        if stop is not None:
            stop.wait(0.5)
        else:
            time.sleep(1)


def locate(path):
    path = os.path.abspath(os.path.expanduser(path))
    for sock, status in snapshots():
        root = status["mountpoint"]
        if path == root or path.startswith(root.rstrip("/") + "/"):
            return sock, "" if path == root else os.path.relpath(path, root), status
    raise RuntimeError("按需同步客户端未运行或目录未挂载")
