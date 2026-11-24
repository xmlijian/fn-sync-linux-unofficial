"""Bounded local IPC client shared by CLI and file manager. Never accesses FUSE."""
import glob
import json
import os
import socket


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


def snapshots():
    result = []
    base = os.path.expanduser("~/.local/share/fn-sync-client/ondemand/*/control.sock")
    for sock in glob.glob(base):
        try:
            result.append((sock, request(sock, "status")))
        except (OSError, ValueError, RuntimeError):
            continue
    return result


def locate(path):
    path = os.path.abspath(os.path.expanduser(path))
    for sock, status in snapshots():
        root = status["mountpoint"]
        if path == root or path.startswith(root.rstrip("/") + "/"):
            return sock, "" if path == root else os.path.relpath(path, root), status
    raise RuntimeError("按需同步客户端未运行或目录未挂载")
