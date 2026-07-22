"""Cloud-transport test against a local mock of smolfleet /v1 (no VM, no real net).

Mirrors the Node ``test/cloud-mock.ts``: verifies the wire shapes the SDK sends
(tagged source, nested camelCase resources, network.mode, argv command, camelCase
exec response) and the routes/verbs, plus NotSupported gating.
"""

import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))

from smol import ConnectOptions, Machine, MachineConfig, MountSpec, NotSupportedError, PortSpec, ResourceSpec, SmolError  # noqa: E402
from smol.transport import _cloud_fetch  # noqa: E402  (internal — asserts request-id surfacing)

MACHINE_ID = "mach-test123"
CLONE_ID = "mach-clone456"
captured: dict = {"hits": [], "create_body": None, "exec_body": None, "file": None}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):  # silence
        pass

    def _auth_ok(self) -> bool:
        return self.headers.get("authorization") == "Bearer smk_testkey"

    def _send(self, code: int, body: bytes = b"", ctype: str = "application/json"):
        self.send_response(code)
        # The real control plane sets x-request-id on every response; mirror it
        # so the SDK's error-message surfacing can be asserted.
        self.send_header("x-request-id", "req-test-xyz")
        if body:
            self.send_header("content-type", ctype)
        self.end_headers()
        if body:
            self.wfile.write(body)

    def _read(self) -> bytes:
        n = int(self.headers.get("content-length", 0))
        return self.rfile.read(n) if n else b""

    def do_POST(self):
        captured["hits"].append(f"POST {self.path}")
        if not self._auth_ok():
            return self._send(401, b"bad token")
        if self.path == "/v1/machines":
            captured["create_body"] = json.loads(self._read() or b"{}")
            return self._send(201, json.dumps({
                "id": MACHINE_ID, "name": captured["create_body"].get("name") or "auto",
                "source": captured["create_body"].get("source"), "state": "stopped",
                "resources": {"cpus": 1, "memoryMb": 512}, "network": {"mode": "blocked"},
                "env": {}, "ephemeral": False,
                "createdAt": "2026-05-30T00:00:00Z", "updatedAt": "2026-05-30T00:00:00Z",
            }).encode())
        if self.path.startswith(f"/v1/machines/{MACHINE_ID}/start"):
            captured["start_path"] = self.path  # carries ?forkable=true when set
            return self._send(200, json.dumps({"id": MACHINE_ID, "state": "started"}).encode())
        if self.path == f"/v1/machines/{MACHINE_ID}/fork":
            captured["fork_body"] = json.loads(self._read() or b"{}")
            return self._send(201, json.dumps({
                "id": CLONE_ID, "name": captured["fork_body"].get("name") or "clone",
                "source": {"type": "image", "reference": "alpine:3.20"}, "state": "started",
                "resources": {"cpus": 2, "memoryMb": 1024}, "network": {"mode": "open"},
                "env": {}, "ephemeral": False, "ports": captured["fork_body"].get("ports") or [],
                "createdAt": "2026-05-30T00:00:00Z", "updatedAt": "2026-05-30T00:00:00Z",
            }).encode())
        if self.path == f"/v1/machines/{MACHINE_ID}/exec":
            captured["exec_body"] = json.loads(self._read() or b"{}")
            return self._send(200, json.dumps({
                "stdout": "hello\n", "stderr": "", "exitCode": 0,
                "durationMs": 12, "machineId": MACHINE_ID,
                "stdoutTruncated": True, "stderrTruncated": False,
            }).encode())
        if self.path == f"/v1/machines/{MACHINE_ID}/stop":
            return self._send(200, json.dumps({"id": MACHINE_ID, "state": "stopped"}).encode())
        return self._send(404, b"no route")

    def do_GET(self):
        captured["hits"].append(f"GET {self.path}")
        if not self._auth_ok():
            return self._send(401, b"bad token")
        if self.path == f"/v1/machines/{MACHINE_ID}":
            return self._send(200, json.dumps({
                "id": MACHINE_ID, "state": "started",
                "ready": True, "readyAt": "2026-07-22T20:01:41.152Z",
            }).encode())
        if self.path == f"/v1/machines/{CLONE_ID}":
            return self._send(200, json.dumps({"id": CLONE_ID, "state": "started", "ready": True}).encode())
        # The connect bridge: GET /v1/machines/:id/connect/:port[/rest]. Echo the
        # path + auth so the SDK's endpoint()/request() wiring can be asserted.
        if self.path.startswith(f"/v1/machines/{MACHINE_ID}/connect/"):
            captured["connect_path"] = self.path
            return self._send(200, json.dumps({"ok": True, "path": self.path}).encode())
        if self.path.startswith(f"/v1/machines/{MACHINE_ID}/files/"):
            return self._send(200, captured.get("file") or b"", "application/octet-stream")
        return self._send(404, b"no route")

    def do_PUT(self):
        captured["hits"].append(f"PUT {self.path}")
        if not self._auth_ok():
            return self._send(401, b"bad token")
        if self.path.startswith(f"/v1/machines/{MACHINE_ID}/files/"):
            captured["file"] = self._read()
            return self._send(200, b"{}")
        return self._send(404, b"no route")

    def do_DELETE(self):
        captured["hits"].append(f"DELETE {self.path}")
        if not self._auth_ok():
            return self._send(401, b"bad token")
        if self.path == f"/v1/machines/{MACHINE_ID}":
            return self._send(204)
        return self._send(404, b"no route")


def main() -> int:
    server = HTTPServer(("127.0.0.1", 0), Handler)
    port = server.server_address[1]
    t = threading.Thread(target=server.serve_forever, daemon=True)
    t.start()
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

    try:
        m = Machine.create(
            MachineConfig(image="alpine:3.20", forkable=True, env={"FOO": "bar"}, workdir="/app",
                          resources=ResourceSpec(cpus=2, memory_mb=1024, network=True)),
            ConnectOptions(target="cloud", base_url=base, api_key="smk_testkey"),
        )
        cb = captured["create_body"]
        check("create hit POST /v1/machines", "POST /v1/machines" in captured["hits"])
        check("source is tagged image reference",
              cb["source"] == {"type": "image", "reference": "alpine:3.20"}, str(cb.get("source")))
        check("resources nested camelCase", cb["resources"].get("cpus") == 2 and cb["resources"].get("memoryMb") == 1024,
              str(cb.get("resources")))
        check("network mode open", cb.get("network") == {"mode": "open"}, str(cb.get("network")))
        check("create sends env as a plain map", cb.get("env") == {"FOO": "bar"}, str(cb.get("env")))
        check("create sends workdir", cb.get("workdir") == "/app", str(cb.get("workdir")))
        check("waited for ready (GET machine)", any(h.startswith(f"GET /v1/machines/{MACHINE_ID}") for h in captured["hits"]))
        check("name from response", m.name == "auto", m.name)
        check("forkable start passes ?forkable=true", "forkable=true" in captured.get("start_path", ""),
              captured.get("start_path"))

        # --- readiness: `started` but the unambiguous ready flag is separate ---
        check("state() over REST", m.state() == "started", m.state())
        check("ready() reads the readiness flag", m.ready() is True)
        check("ready_at() reads the readiness timestamp",
              m.ready_at() == "2026-07-22T20:01:41.152Z", str(m.ready_at()))
        m.wait_until_ready(timeout_s=2, interval_s=0.05)
        check("wait_until_ready() resolves on ready", True)

        # --- connect bridge: authed endpoint URL + request to a published port ---
        ep = m.endpoint(80)
        check("endpoint() builds the connect-bridge http_url",
              ep.http_url == f"{base}/v1/machines/{MACHINE_ID}/connect/80", ep.http_url)
        check("endpoint() derives a ws url",
              ep.ws_url == f"{base.replace('http', 'ws', 1)}/v1/machines/{MACHINE_ID}/connect/80", ep.ws_url)
        check("endpoint() carries Bearer auth", ep.headers.get("authorization") == "Bearer smk_testkey",
              str(ep.headers))
        check("endpoint(port, path) appends the sub-path",
              m.endpoint(80, "/healthz").http_url == f"{base}/v1/machines/{MACHINE_ID}/connect/80/healthz",
              m.endpoint(80, "/healthz").http_url)
        body = json.loads(m.request(80, "healthz").decode())
        check("request() reaches the guest port through the authed bridge",
              body.get("ok") is True and captured.get("connect_path") == f"/v1/machines/{MACHINE_ID}/connect/80/healthz",
              str(captured.get("connect_path")))

        # --- fork: live-RAM RL clone over the cloud ---
        clone = m.fork("rollout-1", ports=[PortSpec(host=18080, guest=80)])
        check("fork hit POST /fork", f"POST /v1/machines/{MACHINE_ID}/fork" in captured["hits"])
        check("fork body carries clone name", captured["fork_body"].get("name") == "rollout-1",
              str(captured.get("fork_body")))
        check("fork ports mapped guest+hostPort",
              captured["fork_body"].get("ports") == [{"port": 80, "hostPort": 18080}],
              str(captured["fork_body"].get("ports")))
        check("fork returns running clone handle", clone.name == "rollout-1" and clone.state() == "started",
              f"{clone.name}/{clone.state()}")

        r = m.exec(["echo", "hello"], )
        check("exec hit POST /exec", f"POST /v1/machines/{MACHINE_ID}/exec" in captured["hits"])
        check("exec command sent as argv array", captured["exec_body"]["command"] == ["echo", "hello"],
              str(captured["exec_body"].get("command")))
        check("exec result maps camelCase", r.exit_code == 0 and r.stdout == "hello\n" and r.success is True)
        check("exec surfaces truncation flags",
              r.stdout_truncated is True and r.stderr_truncated is False,
              f"{r.stdout_truncated}/{r.stderr_truncated}")

        m.write_file("/tmp/a b.txt", "payload")
        back = m.read_file("/tmp/a b.txt")
        check("file round-trip over REST (encoded path)", back == b"payload", repr(back))

        try:
            m.run("alpine", ["echo", "x"])
            check("run gated NotSupported on cloud", False, "did not raise")
        except NotSupportedError:
            check("run gated NotSupported on cloud", True)

        try:
            Machine.create(
                MachineConfig(image="alpine:3.20", mounts=[MountSpec(source="/data", target="/data")]),
                ConnectOptions(target="cloud", base_url=base, api_key="smk_testkey"),
            )
            check("cloud create rejects host mounts", False, "did not raise")
        except NotSupportedError:
            check("cloud create rejects host mounts", True)

        # Published ports ARE a cloud feature: create sends only the guest port;
        # the control plane allocates the node host port. (Contrast: mounts above.)
        Machine.create(
            MachineConfig(image="alpine:3.20", ports=[PortSpec(host=8080, guest=80)]),
            ConnectOptions(target="cloud", base_url=base, api_key="smk_testkey"),
        )
        check("cloud create publishes ports (guest port only; hostPort allocated)",
              captured["create_body"].get("ports") == [{"port": 80}],
              str(captured["create_body"].get("ports")))
        check("env/workdir omitted from the body when unset",
              "env" not in captured["create_body"] and "workdir" not in captured["create_body"],
              str(captured["create_body"]))

        # Errors surface the server's x-request-id so support can correlate the
        # call (clients see the error body but not response headers).
        rid_msg = ""
        try:
            _cloud_fetch(base, "smk_testkey", "GET", "/v1/does-not-exist")
        except SmolError as e:
            rid_msg = str(e)
        check("error surfaces x-request-id", "[request id: req-test-xyz]" in rid_msg, rid_msg)

        m.stop()
        check("stop hit POST /stop", f"POST /v1/machines/{MACHINE_ID}/stop" in captured["hits"])
        m.delete()
        check("delete uses DELETE /v1/machines/{id}", f"DELETE /v1/machines/{MACHINE_ID}" in captured["hits"])
    finally:
        server.shutdown()

    print(f"\n{passed} passed, {failed} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
