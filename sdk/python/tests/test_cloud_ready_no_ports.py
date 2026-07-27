"""Regression: a cloud machine with NO published port never flips the `ready`
flag on control planes that gate readiness on a port accepting a connection.
`create()` must not hang the full timeout — it confirms the guest agent is
reachable (a trivial exec) and returns. This is the SDK's most basic call
(a compute sandbox you only exec into)."""

import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))

from smol import ConnectOptions, Machine, MachineConfig  # noqa: E402

MID = "mach-noport"
seen = {"exec_probe": False}


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
            # No ports on the record.
            return self._send(201, json.dumps({"id": MID, "name": "np", "state": "stopped", "ports": []}).encode())
        if self.path == f"/v1/machines/{MID}/start":
            return self._send(200, json.dumps({"id": MID, "state": "starting"}).encode())
        if self.path == f"/v1/machines/{MID}/exec":
            # The readiness probe: agent reachable → 200.
            seen["exec_probe"] = True
            n = int(self.headers.get("content-length", 0))
            self.rfile.read(n)
            return self._send(200, json.dumps({"exitCode": 0, "stdout": "", "stderr": ""}).encode())
        return self._send(404)

    def do_GET(self):
        if self.path == f"/v1/machines/{MID}":
            # started, but `ready` STAYS false and there are no ports.
            return self._send(200, json.dumps({"id": MID, "state": "started", "ready": False, "ports": []}).encode())
        return self._send(404)

    def do_DELETE(self):
        return self._send(204)


def main() -> int:
    srv = HTTPServer(("127.0.0.1", 0), Handler)
    port = srv.server_address[1]
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{port}"

    passed = failed = 0

    def check(name, cond, detail=""):
        nonlocal passed, failed
        if cond:
            passed += 1
            print(f"  ok {name}")
        else:
            failed += 1
            print(f"  FAIL {name} {detail}")

    result = {}

    def do_create():
        try:
            result["m"] = Machine.create(
                MachineConfig(image="alpine"),
                ConnectOptions(target="cloud", base_url=base, api_key="smk_t"),
            )
        except Exception as e:  # noqa: BLE001
            result["err"] = e

    # Bound it so a regression to the hang fails fast instead of blocking CI 120s.
    t = threading.Thread(target=do_create, daemon=True)
    t.start()
    t.join(timeout=20)

    check("create() returned (did not hang on the never-flipping ready flag)", not t.is_alive() and "m" in result,
          "still running" if t.is_alive() else str(result.get("err")))
    check("readiness was confirmed via the agent exec probe", seen["exec_probe"])

    srv.shutdown()
    print(f"\n{passed} passed, {failed} failed")
    print("RESULT=PASS" if failed == 0 else "RESULT=FAIL")
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
