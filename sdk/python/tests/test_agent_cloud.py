"""Managed-agent contract against a local mock control plane."""

import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))
from smol import AgentSession, ConnectOptions  # noqa: E402

seen = []
info = {"name": "fixer", "harness": "claude-code", "status": "ready", "machineId": "mach-1", "turns": [], "createdAt": "2026-01-01T00:00:00Z"}
turn = {"index": 0, "prompt": "fix tests", "status": "done", "isError": False, "checkpointed": True, "startedAt": "2026-01-01T00:00:00Z"}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def _reply(self, body):
        payload = json.dumps(body).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def _handle(self):
        length = int(self.headers.get("content-length", "0"))
        body = self.rfile.read(length).decode() if length else ""
        seen.append((self.command, self.path, body, self.headers.get("Idempotency-Key")))
        if self.path.endswith("/events"):
            payload = ("event: event\r\nid: 7\r\ndata: {\"type\":\"text\"}\r\n\r\n" +
                       "event: done\ndata: " + json.dumps(turn) + "\n\n").encode()
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        elif self.path == "/v1/machines/mach-1":
            self._reply({"id": "mach-1", "name": "fixer-vm"})
        elif self.path == "/v1/agents/fixer/turns":
            self._reply({"turn": 0})
        elif self.path == "/v1/agents/fixer/fork":
            self._reply({**info, "name": "alternative"})
        elif self.path == "/v1/agents" and self.command == "GET":
            self._reply({"items": [info]})
        elif self.command == "DELETE" or self.path.endswith(("/cancel", "/pause", "/resume")):
            self.send_response(204)
            self.end_headers()
        else:
            self._reply(info)

    do_GET = do_POST = do_DELETE = _handle


def test_agent_cloud_contract():
    server = HTTPServer(("127.0.0.1", 0), Handler)
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    try:
        conn = ConnectOptions(target="cloud", base_url=f"http://127.0.0.1:{server.server_port}", api_key="smk_test")
        session = AgentSession.create("fixer", credential="anthropic", conn=conn)
        assert session.info()["status"] == "ready"
        assert session.machine().id == "mach-1"
        assert AgentSession.list(conn)["items"][0]["name"] == "fixer"
        assert session.send("fix tests", idempotency_key="task-1") == 0
        events = list(session.events(0))
        assert [event["type"] for event in events] == ["event", "done"]
        assert events[0]["id"] == 7
        assert session.fork(0, "alternative").name == "alternative"
        session.cancel(0)
        session.pause()
        session.resume()
        session.delete()
        assert next(item for item in seen if item[1] == "/v1/agents/fixer/turns")[3] == "task-1"
        assert json.loads(seen[0][2])["credential"] == "anthropic"
        assert all(item[1].startswith("/v1/agents") or item[1] == "/v1/machines/mach-1" for item in seen)
    finally:
        server.shutdown()
        server.server_close()
        worker.join()


if __name__ == "__main__":
    test_agent_cloud_contract()
    print("managed agent SDK cloud contract passed")
