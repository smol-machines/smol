"""Regression: a cloud branch_batch waits for its children's readiness in
parallel. Waiting one child at a time cost one round trip per child after the
control plane had already answered, which made a 32-child batch take 2.5 times
as long as the server did."""

import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))

from smol import ConnectOptions, Machine  # noqa: E402

SOURCE = "mach-source"
CHILDREN = [f"mach-child-{i}" for i in range(8)]
# Each readiness check takes this long, so waiting one child at a time would
# take len(CHILDREN) * READY_DELAY_S.
READY_DELAY_S = 0.25


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):  # silence
        pass

    def _send(self, code: int, body: bytes = b""):
        self.send_response(code)
        if body:
            self.send_header("content-type", "application/json")
        self.end_headers()
        if body:
            self.wfile.write(body)

    def do_POST(self):
        n = int(self.headers.get("content-length", 0))
        self.rfile.read(n)
        if self.path == f"/v1/machines/{SOURCE}/branches/batch":
            clones = [{"id": c, "name": c, "state": "started"} for c in CHILDREN]
            return self._send(201, json.dumps({"clones": clones}).encode())
        return self._send(404)

    def do_GET(self):
        mid = self.path.rsplit("/", 1)[-1]
        if mid == SOURCE:
            return self._send(200, json.dumps({"id": SOURCE, "name": "src", "state": "started", "ready": True}).encode())
        if mid in CHILDREN:
            time.sleep(READY_DELAY_S)
            return self._send(200, json.dumps({"id": mid, "name": mid, "state": "started", "ready": True}).encode())
        return self._send(404)


def main() -> int:
    srv = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{srv.server_address[1]}"

    passed = failed = 0

    def check(name, cond, detail=""):
        nonlocal passed, failed
        if cond:
            passed += 1
            print(f"  ok {name}")
        else:
            failed += 1
            print(f"  FAIL {name} {detail}")

    source = Machine.connect(SOURCE, ConnectOptions(target="cloud", base_url=base, api_key="smk_t"))
    start = time.monotonic()
    children = source.branch_batch(names=CHILDREN)
    elapsed = time.monotonic() - start
    sequential = len(CHILDREN) * READY_DELAY_S

    check("branch_batch returned every child", len(children) == len(CHILDREN), f"got {len(children)}")
    check(
        "children were awaited in parallel",
        elapsed < sequential / 2,
        f"took {elapsed:.2f}s; one at a time would take {sequential:.2f}s",
    )

    srv.shutdown()
    print(f"\n{passed} passed, {failed} failed")
    print("RESULT=PASS" if failed == 0 else "RESULT=FAIL")
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
