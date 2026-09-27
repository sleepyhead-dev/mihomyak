#!/usr/bin/env python3
"""A subscription "panel" that misbehaves on purpose (stdlib only).

mihomyak must neither hang past its request deadline nor crash on any of these,
and the working config must stay in place. The path picks the behaviour:

    /slow        valid headers, then one body byte every 2 s, forever
    /trickle     one header byte every 2 s, forever
    /hang        accept the connection and never answer
    /huge        endless body without Content-Length
    /bighead     an endless header line
    /manyheaders endless stream of short headers
    /badchunk    chunked body with an invalid chunk size
    /hugechunk   chunked body announcing a 1 TiB chunk
    /gzipbomb    small gzip that inflates to 1 GiB of zeros
    /redirect    302 to http:// (https -> http downgrade when served over TLS)
    /loop        endless redirects to itself
    /garbage     not HTTP at all

Usage: python3 hostile_panel.py [--host 0.0.0.0] [--port 8081]
"""

import argparse
import gzip
import io
import socketserver
import time


def gzip_bomb(size=1 << 30):
    buf = io.BytesIO()
    with gzip.GzipFile(fileobj=buf, mode="wb", compresslevel=9) as gz:
        chunk = b"\0" * (1 << 20)
        for _ in range(size // len(chunk)):
            gz.write(chunk)
    return buf.getvalue()


BOMB = None


class Handler(socketserver.StreamRequestHandler):
    def send(self, data):
        self.wfile.write(data)
        self.wfile.flush()

    def forever(self, piece, pause):
        while True:
            self.send(piece)
            if pause:
                time.sleep(pause)

    def handle(self):
        global BOMB
        line = self.rfile.readline(8192).decode("latin-1")
        while self.rfile.readline(8192) not in (b"\r\n", b"\n", b""):
            pass
        path = line.split(" ")[1] if line.count(" ") >= 2 else "/"
        path = path.split("?")[0].rstrip("/") or "/"
        print(f"{time.strftime('%H:%M:%S')} {path}", flush=True)
        ok = b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n"
        try:
            if path.endswith("/slow"):
                self.send(ok + b"content-length: 1000000\r\n\r\n")
                self.forever(b"a", 2)
            elif path.endswith("/trickle"):
                self.forever(b"H", 2)
            elif path.endswith("/hang"):
                time.sleep(3600)
            elif path.endswith("/huge"):
                self.send(ok + b"connection: close\r\n\r\n")
                self.forever(b"proxies: []\n" * 8192, 0)
            elif path.endswith("/bighead"):
                self.send(ok + b"x-big: ")
                self.forever(b"a" * 8192, 0)
            elif path.endswith("/manyheaders"):
                self.send(ok)
                self.forever(b"x-a: b\r\n", 0)
            elif path.endswith("/badchunk"):
                self.send(ok + b"transfer-encoding: chunked\r\n\r\nzz\r\nabc\r\n0\r\n\r\n")
            elif path.endswith("/hugechunk"):
                self.send(ok + b"transfer-encoding: chunked\r\n\r\n10000000000\r\n")
                self.forever(b"a" * 8192, 0)
            elif path.endswith("/gzipbomb"):
                BOMB = BOMB or gzip_bomb()
                self.send(ok + b"content-encoding: gzip\r\ncontent-length: %d\r\n\r\n" % len(BOMB) + BOMB)
            elif path.endswith("/redirect"):
                self.send(b"HTTP/1.1 302 Found\r\nlocation: http://198.51.100.10:8080/sub/redirected\r\n"
                          b"content-length: 0\r\n\r\n")
            elif path.endswith("/loop"):
                self.send(b"HTTP/1.1 302 Found\r\nlocation: " + path.encode() + b"\r\ncontent-length: 0\r\n\r\n")
            elif path.endswith("/garbage"):
                self.send(b"\x16\x03\x01garbage\r\n\r\n" * 100)
            else:
                self.send(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n")
        except (BrokenPipeError, ConnectionResetError):
            pass


class Server(socketserver.ThreadingTCPServer):
    daemon_threads = True
    allow_reuse_address = True


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8081)
    args = parser.parse_args()
    print(f"hostile panel on http://{args.host}:{args.port}/sub/<mode>", flush=True)
    Server((args.host, args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
