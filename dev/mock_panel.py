#!/usr/bin/env python3
"""Local stand-in for a Remnawave subscription endpoint (stdlib only).

Reproduces the behaviour documented in docs/SUBSCRIPTIONS.md §2 so mihomyak can be
exercised without a real subscription:

* default Response Rules: browser → HTML, mihomo-family UA → YAML, else base64 links,
  empty UA → 403;
* HWID device limit: `x-hwid` validated with ^[a-zA-Z0-9=-]{10,64}$, devices
  registered up to --device-limit, refusals answered with x-hwid-* headers and
  `0.0.0.0:1` remark stubs;
* provider headers: subscription-userinfo, profile-title (base64:), interval, …;
* control endpoints to flip states while a client is running.

Usage:
    python3 dev/mock_panel.py --port 8080 --device-limit 1 --proxy ss://…@host:port
    curl -X POST 'http://127.0.0.1:8080/_control?state=expired'   # good|expired|limited
    curl -X POST 'http://127.0.0.1:8080/_control?reset=1'         # forget devices
    curl 'http://127.0.0.1:8080/_log'                              # requests seen
"""

import argparse
import base64
import json
import re
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HWID_RE = re.compile(r"^[a-zA-Z0-9=-]{10,64}$")
MIHOMO_UA = re.compile(
    r"^(?:flclash|rabbit|flowvy|murge|mihomo|prizrak-box|koala-clash|"
    r"clash(?:-verge|-nyanpasu|x meta|[-.]?meta))",
    re.I,
)
REMARKS = {
    "expired": ["⌛ Subscription expired", "Contact support"],
    "limited": ["🚧 Subscription limited", "Contact support"],
    "hwid_max": ["Limit of devices reached"],
    "hwid_unsupported": ["App not supported"],
}


def b64(text: str) -> str:
    return base64.b64encode(text.encode()).decode()


class Panel:
    def __init__(self, args):
        self.args = args
        self.state = "good"
        self.devices = {}  # hwid -> headers
        self.log = []
        self.lock = threading.Lock()

    def proxies(self):
        """(name, share link) pairs served to clients."""
        if self.args.proxy:
            return [(f"Real {i + 1}", link) for i, link in enumerate(self.args.proxy)]
        return [
            ("🇳🇱 Netherlands", "vless://11111111-2222-3333-4444-555555555555@nl.example.com:443"
             "?security=reality&sni=www.google.com&fp=chrome&pbk=Z84J2IelR9ch3k8VtlVhhs5ycBUlXA7wHBWcBrjqnAw"
             "&sid=6ba85179e30d4fc2&type=tcp&flow=xtls-rprx-vision"),
            ("🇩🇪 Germany", "trojan://secret@de.example.com:443?sni=de.example.com"),
        ]


def yaml_config(proxies, remarks=None):
    """Mihomo template output, shaped like Remnawave's default template."""
    lines = ["mixed-port: 7890", "socks-port: 7891", "allow-lan: true", "mode: global",
             "log-level: info", "external-controller: 127.0.0.1:9090", "proxies:"]
    names = []
    if remarks is not None:
        for remark in remarks:
            names.append(remark)
            lines += [f"  - name: {json.dumps(remark)}", "    type: vless", "    server: 0.0.0.0",
                      "    port: 1", "    uuid: 00000000-0000-0000-0000-000000000000",
                      "    network: tcp", "    udp: true"]
    else:
        for name, link in proxies:
            names.append(name)
            lines.append(f"  - {json.dumps(link_to_mihomo(name, link))}")
    lines += ["proxy-groups:", "  - name: '→ Remnawave'", "    type: select", "    proxies:"]
    lines += [f"      - {json.dumps(n)}" for n in names]
    lines += ["rules:", "  - MATCH,→ Remnawave", ""]
    return "\n".join(lines)


def link_to_mihomo(name, link):
    """Just enough conversion for the links the mock serves."""
    u = urllib.parse.urlsplit(link)
    q = dict(urllib.parse.parse_qsl(u.query))
    if u.scheme == "ss":
        userinfo = urllib.parse.unquote(u.netloc.rsplit("@", 1)[0])
        if ":" not in userinfo:
            userinfo = base64.urlsafe_b64decode(userinfo + "=" * (-len(userinfo) % 4)).decode()
        cipher, password = userinfo.split(":", 1)
        return {"name": name, "type": "ss", "server": u.hostname, "port": u.port,
                "cipher": cipher, "password": password, "udp": True}
    if u.scheme == "trojan":
        return {"name": name, "type": "trojan", "server": u.hostname, "port": u.port,
                "password": urllib.parse.unquote(u.username or ""), "sni": q.get("sni", u.hostname)}
    proxy = {"name": name, "type": "vless", "server": u.hostname, "port": u.port,
             "uuid": u.username, "network": q.get("type", "tcp"), "udp": True}
    if q.get("security") == "reality":
        proxy.update({"tls": True, "servername": q.get("sni"), "flow": q.get("flow", ""),
                      "client-fingerprint": q.get("fp", "chrome"),
                      "reality-opts": {"public-key": q.get("pbk"), "short-id": q.get("sid", "")}})
    return proxy


def links_body(proxies, remarks=None):
    if remarks is not None:
        lines = [f"vless://00000000-0000-0000-0000-000000000000@0.0.0.0:1?security=none&type=tcp#"
                 f"{urllib.parse.quote(r)}" for r in remarks]
    else:
        lines = [f"{link}#{urllib.parse.quote(name)}" for name, link in proxies]
    return b64("\n".join(lines))


def make_handler(panel: Panel):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, fmt, *args):  # quiet
            pass

        def send(self, status, body=b"", headers=None, ctype="text/plain; charset=utf-8"):
            if isinstance(body, str):
                body = body.encode()
            self.send_response(status)
            self.send_header("content-type", ctype)
            for k, v in (headers or {}).items():
                self.send_header(k, v)
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            url = urllib.parse.urlsplit(self.path)
            q = dict(urllib.parse.parse_qsl(url.query))
            if url.path != "/_control":
                return self.send(404, "not found")
            with panel.lock:
                if "state" in q:
                    panel.state = q["state"]
                if q.get("reset"):
                    panel.devices.clear()
            self.send(200, json.dumps({"state": panel.state, "devices": list(panel.devices)}))

        def do_GET(self):
            url = urllib.parse.urlsplit(self.path)
            if url.path == "/_log":
                with panel.lock:
                    return self.send(200, json.dumps(panel.log, ensure_ascii=False, indent=2),
                                     ctype="application/json")
            if not url.path.startswith("/sub/"):
                return self.send(404, "Not Found")
            headers = {k.lower(): v for k, v in self.headers.items()}
            with panel.lock:
                panel.log.append({"time": time.time(), "path": url.path,
                                  "headers": [[k, v] for k, v in self.headers.items()]})
            ua = headers.get("user-agent", "")
            if not ua:
                return self.send(403, "Forbidden")
            if "text/html" in headers.get("accept", ""):
                return self.send(200, "<!doctype html><html><body>sub page</body></html>",
                                 ctype="text/html")
            response_type = "MIHOMO" if MIHOMO_UA.search(ua) else "XRAY_BASE64"
            usage = "upload=0; download=5368709120; total=107374182400; expire=%d" % (
                time.time() + 30 * 86400)
            info = {
                "subscription-userinfo": usage,
                "profile-title": "base64:" + b64("Mock VPN"),
                "profile-update-interval": str(panel.args.interval),
                "support-url": "https://t.me/mock_support",
                "profile-web-page-url": f"http://{self.headers.get('host')}{url.path}",
                "content-disposition": "attachment; filename=mockuser",
            }

            def respond(remarks=None, extra=None):
                if response_type == "MIHOMO":
                    body, ctype = yaml_config(panel.proxies(), remarks), "text/yaml; charset=utf-8"
                else:
                    body, ctype = links_body(panel.proxies(), remarks), "text/plain; charset=utf-8"
                self.send(200, body, {**info, **(extra or {})}, ctype)

            with panel.lock:
                state = panel.state
            if state in ("expired", "limited"):
                return respond(REMARKS[state])

            if panel.args.device_limit > 0:
                hwid = headers.get("x-hwid", "")
                refusal = {"x-hwid-active": "true", "x-hwid-limit": "true"}
                if not HWID_RE.match(hwid):
                    refusal["x-hwid-not-supported"] = "true"
                    if panel.args.show_remarks:
                        return respond(REMARKS["hwid_unsupported"], refusal)
                    return self.send(200, b"", {**info, **refusal})
                with panel.lock:
                    known = hwid in panel.devices
                    full = len(panel.devices) >= panel.args.device_limit
                    if not known and not full:
                        panel.devices[hwid] = {k: headers.get(k) for k in
                                               ("x-device-os", "x-ver-os", "x-device-model", "user-agent")}
                if not known and full:
                    refusal["x-hwid-max-devices-reached"] = "true"
                    refusal["announce"] = "base64:" + b64("Лимит устройств исчерпан")
                    if panel.args.show_remarks:
                        return respond(REMARKS["hwid_max"], refusal)
                    return self.send(200, b"", {**info, **refusal})
                return respond(None, {"x-hwid-active": "true"})
            return respond()

    return Handler


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8080)
    parser.add_argument("--device-limit", type=int, default=1, help="0 disables the HWID check")
    parser.add_argument("--show-remarks", action="store_true", default=True,
                        help="answer refusals with remark stubs (Remnawave isShowCustomRemarks)")
    parser.add_argument("--no-remarks", dest="show_remarks", action="store_false")
    parser.add_argument("--interval", type=int, default=12, help="profile-update-interval, hours")
    parser.add_argument("--proxy", action="append", help="share link to serve (repeatable)")
    args = parser.parse_args()
    server = ThreadingHTTPServer((args.host, args.port), make_handler(Panel(args)))
    print(f"mock panel on http://{args.host}:{args.port}/sub/<any-token>", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
