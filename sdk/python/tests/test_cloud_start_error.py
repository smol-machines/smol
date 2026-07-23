"""Regression: when a cloud machine fails to start and then enters `error`, the
SDK must surface WHY start failed (the machine record carries no error detail) —
not just an opaque "entered error state" — and still delete the orphan."""

import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))

from smol import ConnectOptions, Machine, MachineConfig  # noqa: E402
from smol.errors import SmolError  # noqa: E402

MID = "mach-starterr"
REASON = "pull image: crane manifest failed: manifest unknown"
seen = {"deleted": False}


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
        if self.path == "/v1/machines":
            n = int(self.headers.get("content-length", 0))
            self.rfile.read(n)
            return self._send(201, json.dumps({"id": MID, "name": "boom", "state": "stopped"}).encode())
        if self.path == f"/v1/machines/{MID}/start":
            # Start fails with the real reason in the body.
            return self._send(500, json.dumps({"error": REASON}).encode())
        return self._send(404)

    def do_GET(self):
        if self.path == f"/v1/machines/{MID}":
            # The record itself has no error detail — only `state`.
            return self._send(200, json.dumps({"id": MID, "state": "error"}).encode())
        return self._send(404)

    def do_DELETE(self):
        if self.path == f"/v1/machines/{MID}":
            seen["deleted"] = True
            return self._send(204)
        return self._send(404)


def main() -> int:
    srv = HTTPServer(("127.0.0.1", 0), Handler)
    port = srv.server_address[1]
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{port}"

    passed = failed = 0

    def check(name: str, cond: bool, detail: str = ""):
        nonlocal passed, failed
        if cond:
            passed += 1
            print(f"  ok {name}")
        else:
            failed += 1
            print(f"  FAIL {name} {detail}")

    threw = False
    message = ""
    try:
        Machine.create(
            MachineConfig(image="alpine"),
            ConnectOptions(target="cloud", base_url=base, api_key="smk_t"),
        )
    except SmolError as e:
        threw = True
        message = str(e)

    check("create() raised when the machine entered error state", threw)
    check("error message surfaces the start-failure reason", REASON in message, message)
    check("orphan machine was deleted (no leak)", seen["deleted"])

    srv.shutdown()
    print(f"\n{passed} passed, {failed} failed")
    print("RESULT=PASS" if failed == 0 else "RESULT=FAIL")
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
