"""Resumable execs on the cloud target, against a local mock control plane that
cuts off connections mid-command the way a restarting control does.

    python tests/test_cloud_resumable.py
"""

import base64
import json
import sys
import threading
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))

from smol import ConnectOptions, Machine  # noqa: E402
from smol.transport import _resumable_verdict  # noqa: E402

LINES = 30
OUTPUT = "".join(f"line {i}\n" for i in range(LINES)).encode()
seen: dict = {"requests": 0, "starts": [], "deletes": 0, "plain": 0, "file_gets": 0, "refuse_first": True}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):  # silence
        pass

    def _json(self, code: int, obj) -> None:
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _cut(self) -> None:
        # No answer at all: the control died under the request.
        self.close_connection = True
        try:
            import socket

            self.connection.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass

    def _body(self) -> dict:
        n = int(self.headers.get("content-length") or 0)
        return json.loads(self.rfile.read(n) or b"{}")

    def do_GET(self):
        path = self.path
        if path in ("/v1/machines/m-new", "/v1/machines/m-old"):
            return self._json(200, {"id": path.rsplit("/", 1)[1], "state": "started", "ready": True})
        if path.startswith("/v1/machines/m-new/files/") and path.endswith("data.bin"):
            seen["file_gets"] += 1
            if seen["file_gets"] == 1:
                self.send_response(200)
                self.send_header("content-type", "application/octet-stream")
                self.send_header("content-length", "1000")
                self.end_headers()
                self.wfile.write(b"\x07" * 100)
                self.wfile.flush()
                return self._cut()
            body = b"\x07" * 1000
            self.send_response(200)
            self.send_header("content-type", "application/octet-stream")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if path.startswith("/v1/machines/m-new/execs/"):
            seen["requests"] += 1
            if seen["requests"] % 3 == 0:
                return self._cut()
            return self._answer(int(urllib.parse.parse_qs(urllib.parse.urlsplit(path).query)["after"][0]), 200)
        return self._json(404, {"code": "not_found"})

    def do_POST(self):
        path = self.path
        body = self._body()
        if path.startswith("/v1/machines/m-old/execs"):
            return self._json(404, {"code": "not_found", "detail": "no such route"})
        if path == "/v1/machines/m-old/exec":
            seen["plain"] += 1
            return self._json(200, {"exitCode": 0, "stdout": "plain\n", "stderr": ""})
        if path == "/v1/machines/m-new/execs":
            seen["starts"].append(body)
            if seen["refuse_first"] and len(seen["starts"]) == 1:
                return self._json(503, {"code": "control_draining", "retryable": True})
            return self._answer(0, 201)
        return self._json(404, {"code": "not_found"})

    def do_DELETE(self):
        seen["deletes"] += 1
        self.send_response(204)
        self.send_header("content-length", "0")
        self.end_headers()

    def _answer(self, after: int, code: int) -> None:
        end = min(after + 7, len(OUTPUT))
        done = end == len(OUTPUT)
        answer = {
            "execId": "x",
            "state": "exited" if done else "running",
            "done": done,
            "output": [{
                "stream": "stdout" if after % 2 == 0 else "stderr",
                "offset": after,
                "data": base64.b64encode(OUTPUT[after:end]).decode(),
            }],
            "next": end,
            "start": after,
            "end": len(OUTPUT),
        }
        if done:
            answer["exitCode"] = 4
        self._json(code, answer)


def main() -> int:
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{server.server_address[1]}"
    cloud = ConnectOptions(target="cloud", base_url=base, api_key="smk_test")
    failures = []

    def check(label: str, ok: bool, detail: str = "") -> None:
        print(("  ✓ " if ok else "  ✗ ") + label + ("" if ok else f" — {detail}"))
        if not ok:
            failures.append(label)

    m = Machine.connect("m-new", cloud)
    r = m.exec(["build"])
    got = sorted(r.stdout_bytes + r.stderr_bytes)
    check(
        "an exec rides out cut-off connections: every byte once, and the exit code",
        got == sorted(OUTPUT) and r.exit_code == 4,
        f"{len(got)}/{len(OUTPUT)} bytes, exit {r.exit_code}",
    )
    ids = {s["execId"] for s in seen["starts"]}
    check(
        "a start refused while the control drains is retried under the same exec id",
        len(seen["starts"]) == 2 and len(ids) == 1 and seen["starts"][0]["waitSeconds"] == 20,
        json.dumps(seen["starts"]),
    )
    check("the collected exec is deleted", seen["deletes"] == 1, f"deletes={seen['deletes']}")

    seen["refuse_first"] = False
    seen["requests"] = 0
    text, exit_code = "", None
    for ev in m.exec_stream(["build"]):
        if ev["kind"] in ("stdout", "stderr"):
            text += ev["data"]
        elif ev["kind"] == "exit":
            exit_code = ev["exit_code"]
        else:
            raise AssertionError(ev)
    check("a streamed exec delivers its output in order across cut-offs", text == OUTPUT.decode() and exit_code == 4, repr(text[:40]))

    data = m.read_file("/data.bin")
    check("a download cut off midway is read again whole", len(data) == 1000 and seen["file_gets"] == 2, f"len={len(data)} gets={seen['file_gets']}")

    old = Machine.connect("m-old", cloud)
    plain = old.exec(["true"])
    check("a control plane without resumable execs gets the plain exec", plain.stdout == "plain\n" and seen["plain"] == 1)

    check(
        "a start is retried only where it cannot have reached a node",
        _resumable_verdict(503, "control_draining", True) == "retry"
        and _resumable_verdict(503, "unavailable", True) == "fail"
        and _resumable_verdict(503, "unavailable", False) == "retry"
        and _resumable_verdict(503, "machine_interrupted", False) == "fail"
        and _resumable_verdict(404, None, True) == "unsupported"
        and _resumable_verdict(404, "exec_not_found", False) == "fail",
    )
    server.shutdown()
    print(f"\n{7 - len(failures)} passed, {len(failures)} failed")
    return 1 if failures else 0


def test_cloud_resumable() -> None:
    assert main() == 0


if __name__ == "__main__":
    sys.exit(main())
