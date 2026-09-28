#!/usr/bin/env python3
"""Records subscription requests of real clients byte for byte (stdlib only).

Every request is saved raw to <out>/NN.http (head and body, CRLF kept) and summed
up on stdout. The answer is a small, valid Remnawave-like subscription (base64
share links plus the usual provider headers), so clients accept it and carry on.

    python3 capture_server.py --port 18080 --out captures
"""

import argparse
import base64
import os
import socketserver
import threading
import time

LINKS = "\n".join([
    "vless://11111111-2222-3333-4444-555555555555@198.51.100.21:443?security=none&type=tcp#Capture-1",
    "ss://YWVzLTEyOC1nY206Y2FwdHVyZQ@198.51.100.22:8388#Capture-2",
])
HEADERS = [
    ("content-type", "text/plain; charset=utf-8"),
    ("subscription-userinfo", "upload=0; download=0; total=107374182400; expire=0"),
    ("profile-title", "base64:" + base64.b64encode(b"Capture").decode()),
    ("profile-update-interval", "24"),
]


class Handler(socketserver.StreamRequestHandler):
    counter = 0
    lock = threading.Lock()

    def handle(self):
        head = b""
        while not head.endswith(b"\r\n\r\n") and not head.endswith(b"\n\n"):
            byte = self.rfile.read(1)
            if not byte:
                break
            head += byte
            if len(head) > 65536:
                break
        body = b""
        for line in head.split(b"\r\n"):
            if line.lower().startswith(b"content-length:"):
                body = self.rfile.read(int(line.split(b":", 1)[1].strip() or 0))
        with Handler.lock:
            Handler.counter += 1
            number = Handler.counter
        with open(os.path.join(self.server.out, f"{number:02d}.http"), "wb") as f:
            f.write(head + body)
        first = head.split(b"\r\n", 1)[0].decode("latin-1", "replace")
        print(f"{time.strftime('%H:%M:%S')} #{number} {self.client_address[0]} {first}", flush=True)
        payload = base64.b64encode(LINKS.encode())
        response = b"HTTP/1.1 200 OK\r\n"
        for name, value in HEADERS:
            response += f"{name}: {value}\r\n".encode()
        response += f"content-length: {len(payload)}\r\nconnection: close\r\n\r\n".encode()
        self.wfile.write(response + payload)


class Server(socketserver.ThreadingTCPServer):
    daemon_threads = True
    allow_reuse_address = True


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="0.0.0.0")
    parser.add_argument("--port", type=int, default=18080)
    parser.add_argument("--out", default="captures")
    args = parser.parse_args()
    os.makedirs(args.out, exist_ok=True)
    server = Server((args.host, args.port), Handler)
    server.out = args.out
    print(f"capturing on {args.host}:{args.port} into {args.out}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
