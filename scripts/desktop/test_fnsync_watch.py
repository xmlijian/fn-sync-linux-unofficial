"""fnsync_ipc.watch 的订阅/重连逻辑测试（无需真实守护进程）。"""

import os
import socket
import sys
import tempfile
import threading
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.realpath(__file__)))
from fnsync_ipc import watch


class WatchTest(unittest.TestCase):
    def test_receives_ack_and_events(self):
        tmp = tempfile.mkdtemp()
        path = os.path.join(tmp, "control.sock")
        srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        srv.bind(path)
        srv.listen(1)
        received = []
        stop = threading.Event()

        def serve():
            conn, _ = srv.accept()
            with conn:
                conn.recv(1024)
                conn.sendall(b'{"ok":true,"result":{"subscribed":true}}\n')
                conn.sendall(b'{"event":"state"}\n')
                time.sleep(0.3)

        threading.Thread(target=serve, daemon=True).start()
        watcher = threading.Thread(
            target=lambda: watch(path, lambda: received.append(1), stop),
            daemon=True,
        )
        watcher.start()

        deadline = time.time() + 3
        while len(received) < 2 and time.time() < deadline:
            time.sleep(0.05)
        stop.set()
        srv.close()
        watcher.join(timeout=2)

        self.assertGreaterEqual(len(received), 2, "ack + event should trigger callbacks")
        try:
            os.unlink(path)
            os.rmdir(tmp)
        except OSError:
            pass


if __name__ == "__main__":
    unittest.main()
