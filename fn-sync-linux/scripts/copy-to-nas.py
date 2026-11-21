#!/usr/bin/env python3
"""Copy verified artifacts using this client's saved OAuth session; verify SHA-256 by download."""
import argparse
import base64
import hashlib
import hmac
import json
import os
from pathlib import Path
import socket
import sqlite3
import ssl
import struct
import time
import urllib.parse
import urllib.request


def frame(data, opcode=1):
    if isinstance(data, str):
        data = data.encode()
    size = len(data)
    header = bytes([0x80 | opcode])
    header += bytes([0x80 | size]) if size < 126 else (bytes([0xfe]) + struct.pack('>H', size) if size < 65536 else bytes([0xff]) + struct.pack('>Q', size))
    mask = os.urandom(4)
    return header + mask + bytes(c ^ mask[i % 4] for i, c in enumerate(data))


def receive_exact(sock, size):
    data = bytearray()
    while len(data) < size:
        block = sock.recv(size - len(data))
        if not block:
            raise EOFError('NAS closed connection')
        data.extend(block)
    return bytes(data)


class Session:
    def __init__(self, address, account):
        self.address = address
        self.account = dict(account)
        self.key = base64.b64decode(account['secret'])
        url = urllib.parse.urlsplit('https://' + address)
        self.context = ssl._create_unverified_context()
        self.socket = self.context.wrap_socket(socket.create_connection((url.hostname, url.port or 5667), timeout=30), server_hostname=url.hostname)
        request = (f'GET /websocket?type=main HTTP/1.1\r\nHost: {address}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {base64.b64encode(os.urandom(16)).decode()}\r\nSec-WebSocket-Version: 13\r\nCookie: mode=relay\r\n\r\n')
        self.socket.sendall(request.encode())
        response = b''
        while not response.endswith(b'\r\n\r\n'):
            response += receive_exact(self.socket, 1)
        if b' 101 ' not in response.split(b'\r\n')[0]:
            raise RuntimeError('NAS websocket upgrade failed')
        si = self.rpc({'req': 'util.getSI'}, signed=False)
        try:
            result = self.rpc({'req': 'user.authToken', 'token': account['token'], 'si': si.get('si', '')})
        except RuntimeError:
            if not account['long_token']:
                raise RuntimeError('saved NAS session expired; login required') from None
            device_file = Path.home() / '.local/share/fn-sync-client/device_id'
            did = device_file.read_text().strip() if device_file.exists() else 'fn-sync-linux'
            refreshed = self.rpc({'req': 'user.tokenLogin', 'token': account['long_token'],
                'si': si.get('si', ''), 'deviceType': 'PC', 'deviceName': 'Linux-PcSync', 'did': did})
            if refreshed.get('uid') != account['uid'] or not refreshed.get('token'):
                raise RuntimeError('NAS refresh returned an invalid account')
            self.account['token'] = refreshed['token']
            result = self.rpc({'req': 'user.authToken', 'token': refreshed['token'], 'si': si.get('si', '')})
        if result.get('result') != 'succ':
            raise RuntimeError('saved NAS session rejected')

    def signature(self, text):
        return base64.b64encode(hmac.new(self.key, text.encode(), hashlib.sha256).digest()).decode()

    def rpc(self, payload, signed=True):
        payload = dict(payload, reqid=os.urandom(10).hex())
        body = json.dumps(payload, separators=(',', ':'))
        self.socket.sendall(frame((self.signature(body) if signed else '') + body))
        while True:
            op, length = receive_exact(self.socket, 2)
            length &= 0x7f
            if length == 126:
                length = struct.unpack('>H', receive_exact(self.socket, 2))[0]
            elif length == 127:
                length = struct.unpack('>Q', receive_exact(self.socket, 8))[0]
            body = receive_exact(self.socket, length)
            if op & 0x0f == 9:
                self.socket.sendall(frame(body, 10))
                continue
            if op & 0x0f != 1:
                continue
            result = json.loads(body)
            if result.get('reqid') == payload['reqid']:
                if result.get('result') == 'fail':
                    raise RuntimeError(f"NAS request {payload['req']} failed (errno={result.get('errno')})")
                return result

    def copy(self, local, folder):
        data = local.read_bytes()
        remote = folder.rstrip('/') + '/' + local.name
        result = self.rpc({'req': 'file.checkUpload', 'path': remote, 'size': len(data), 'overwrite': False})
        if result.get('result') != 'succ':
            raise RuntimeError('NAS upload preparation failed')
        name = result.get('uploadName', local.name)
        remote = folder.rstrip('/') + '/' + name
        encoded = urllib.parse.quote(remote, safe="/-_.!~*'()")
        boundary = 'fn-sync-' + os.urandom(12).hex()
        body = (f'--{boundary}\r\nContent-Disposition: form-data; name="trim-upload-file"; filename="{name}"\r\nContent-Type: application/octet-stream\r\n\r\n').encode() + data + f'\r\n--{boundary}--\r\n'.encode()
        req = urllib.request.Request('https://' + self.address + '/upload', data=body, headers={
            'Content-Type': 'multipart/form-data; boundary=' + boundary,
            'Trim-Path': encoded, 'Trim-From': '0', 'Trim-Overwrite': 'false',
            'Trim-Mtim': str(int(time.time())), 'Trim-Token': self.account['token'],
            'Trim-Sign': self.signature(encoded), 'Authorization': 'trim ' + self.account['token'],
        })
        with urllib.request.urlopen(req, context=self.context, timeout=120) as response:
            response.read()
        # Large uploads may be listed before their download metadata is ready.
        deadline = time.monotonic() + 30
        while True:
            info = self.rpc({'req': 'file.download', 'files': [remote]})
            entries = info.get('download', [])
            uri = entries[0].get('uri') if entries else None
            if uri:
                break
            if time.monotonic() >= deadline:
                raise RuntimeError('NAS upload finalization timed out')
            time.sleep(0.2)
        url = urllib.parse.urljoin('https://' + self.address, uri)
        if urllib.parse.urlsplit(url).netloc != self.address:
            raise RuntimeError('unexpected NAS download host')
        with urllib.request.urlopen(url, context=self.context, timeout=120) as response:
            actual = hashlib.sha256(response.read()).hexdigest()
        expected = hashlib.sha256(data).hexdigest()
        if actual != expected:
            raise RuntimeError('NAS copy checksum mismatch')
        return {'local': str(local), 'remote': remote, 'bytes': len(data), 'sha256': expected, 'verified': True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--address', required=True)
    parser.add_argument('--folder', required=True)
    parser.add_argument('--db', type=Path, default=Path.home() / '.local/share/fn-sync-client/database.sqlite')
    parser.add_argument('files', nargs='+', type=Path)
    args = parser.parse_args()
    db = sqlite3.connect(f'file:{args.db}?mode=ro', uri=True)
    db.row_factory = sqlite3.Row
    account = db.execute('SELECT * FROM nas_accounts WHERE address=? ORDER BY id DESC LIMIT 1', (args.address,)).fetchone()
    if account is None:
        raise SystemExit('No saved account for requested NAS')
    # Require the destination to be an existing configured sync folder.
    tasks = db.execute('SELECT nas_path FROM tasks WHERE nas_acc_id=?', (account['id'],)).fetchall()
    if args.folder.strip('/') not in [row['nas_path'].strip('/') for row in tasks]:
        raise SystemExit('Destination does not match a configured sync folder')
    session = Session(args.address, account)
    try:
        session.rpc({'req': 'file.ls', 'path': args.folder.strip('/')})
        for local in args.files:
            print(json.dumps(session.copy(local, args.folder.strip('/')), ensure_ascii=False), flush=True)
    finally:
        session.socket.close()


if __name__ == '__main__':
    main()
