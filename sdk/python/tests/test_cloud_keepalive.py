"""Regression: cloud calls reuse keep-alive connections. A new connection per
call paid a TCP and a TLS handshake every time, 193 ms per call from a laptop
against 64 ms on a kept connection, which made a cloud eval trial take five
times as long as the same trial on a local machine."""

import json
import os
import sys
import threading
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))

from smol import transport  # noqa: E402
from smol.errors import SmolError  # noqa: E402

lock = threading.Lock()
connections = 0
posts = 0


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):  # silence
        pass

    def setup(self):
        global connections
        super().setup()
        with lock:
            connections += 1

    def _send(self, code: int, body: dict, close: bool = False):
        raw = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)
        self.wfile.flush()
        if close:
            # Answer as if the connection stays open, then close it, as a
            # server does when an idle keep-alive connection times out.
            self.close_connection = True

    def do_GET(self):
        if self.path == "/conflict":
            return self._send(409, {"code": "conflict", "detail": "busy", "retryable": False})
        return self._send(200, {"ok": True, "path": self.path}, close=self.path == "/close-after")

    def do_POST(self):
        global posts
        n = int(self.headers.get("content-length", 0))
        self.rfile.read(n)
        with lock:
            posts += 1
        return self._send(200, {"ran": True})


class OldHandler(Handler):
    protocol_version = "HTTP/1.0"


def serve(handler):
    srv = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv, f"http://127.0.0.1:{srv.server_address[1]}"


def fetch(base, method, path, body=None):
    return transport._cloud_fetch(base, "smk_t", method, path, json_body=body)


def main() -> int:
    global connections, posts
    passed = failed = 0

    def check(name, cond, detail=""):
        nonlocal passed, failed
        if cond:
            passed += 1
            print(f"  ok {name}")
        else:
            failed += 1
            print(f"  FAIL {name} {detail}")

    srv, base = serve(Handler)

    results = [fetch(base, "GET", f"/v1/machines/m{i}") for i in range(20)]
    check("sequential calls all succeed", all(r and r["ok"] for r in results))
    check("sequential calls share one connection", connections == 1, f"opened {connections}")

    # The server closes the connection after this answer. The next call, a
    # POST that must not run twice, sees the closed socket before sending and
    # opens a new connection.
    connections = 0
    fetch(base, "GET", "/close-after")
    r = fetch(base, "POST", "/v1/machines/m1/exec", {"command": ["true"]})
    check("a POST after the server closed the idle connection succeeds", r == {"ran": True}, repr(r))
    check("that POST ran exactly once", posts == 1, f"ran {posts} times")

    def error_of(call):
        try:
            call()
        except SmolError as e:
            return (e.status, e.code, e.retryable, str(e))
        return None

    headers = {"authorization": "Bearer smk_t"}
    pooled = error_of(lambda: transport._cloud_fetch_pooled(f"{base}/conflict", "GET", "/conflict", None, headers, "json", 10))
    plain = error_of(lambda: transport._cloud_fetch_urllib(f"{base}/conflict", "GET", "/conflict", None, headers, "json", 10))
    check("an HTTP error raises", pooled is not None and pooled[0] == 409, repr(pooled))
    check("an HTTP error matches the urllib path's", pooled == plain, f"{pooled} != {plain}")
    r = fetch(base, "GET", "/v1/machines/after-error")
    check("the connection stays usable after an HTTP error", r and r["ok"], repr(r))

    connections = 0
    with ThreadPoolExecutor(max_workers=16) as pool:
        results = list(pool.map(lambda i: fetch(base, "GET", f"/v1/machines/c{i}"), range(160)))
    check("concurrent calls all succeed", all(r and r["ok"] for r in results))
    check("concurrent calls reuse connections", connections <= 16, f"opened {connections} for 160 calls")
    srv.shutdown()

    old, old_base = serve(OldHandler)
    connections = 0
    results = [fetch(old_base, "GET", f"/v1/machines/o{i}") for i in range(3)]
    check("a server that closes every connection still works", all(r and r["ok"] for r in results))
    check("each of its answers gets a new connection", connections == 3, f"opened {connections}")
    old.shutdown()

    os.environ["http_proxy"] = "http://127.0.0.1:9"
    check("a proxied URL keeps using urllib", not transport._CLOUD_POOL.can_pool("http://api.example.com/v1/me"))
    os.environ["no_proxy"] = "api.example.com"
    check("a URL the proxy is bypassed for is pooled", transport._CLOUD_POOL.can_pool("http://api.example.com/v1/me"))
    del os.environ["http_proxy"], os.environ["no_proxy"]

    print(f"{passed} passed, {failed} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
