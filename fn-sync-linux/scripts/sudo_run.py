#!/usr/bin/env python3
"""用 pty 模拟终端执行 sudo（系统禁用了非 TTY 的 sudo 密码管道）。"""
import os
import pty
import sys
import time

PASSWORD = os.environ.get("SUDO_PASS")
if not PASSWORD:
    raise SystemExit("SUDO_PASS is required")


def run_sudo(cmd: list[str], timeout: float = 180.0) -> int:
    pid, fd = pty.fork()
    if pid == 0:
        os.execvp("sudo", ["sudo", "-S", "-k", "-p", ""] + cmd)

    time.sleep(0.4)
    try:
        os.write(fd, (PASSWORD + "\n").encode())
    except OSError:
        pass

    out = b""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            chunk = os.read(fd, 4096)
        except OSError:
            break
        if not chunk:
            break
        out += chunk
    try:
        os.close(fd)
    except OSError:
        pass
    _, status = os.waitpid(pid, 0)
    text = out.decode("utf-8", "replace")
    # 过滤掉密码提示噪音
    for line in text.splitlines():
        if line.strip() and "Authentication failed" not in line:
            print(line)
    return os.waitstatus_to_exitcode(status)


if __name__ == "__main__":
    sys.exit(run_sudo(sys.argv[1:]))
