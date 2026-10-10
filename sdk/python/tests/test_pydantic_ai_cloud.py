"""Exercise the Harness workspace through Smol's real cloud HTTP transport."""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

import pytest
from pydantic_ai.workspaces import WorkspaceRef
from smol.pydantic_ai_harness import SmolSandbox, SmolSandboxBackend

pytestmark = pytest.mark.anyio


async def test_cloud_lifecycle_and_streamed_command() -> None:
    captured: dict[str, Any] = {}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, format: str, *args: object) -> None:
            pass

        def send_json(self, status: int, data: dict[str, object]) -> None:
            body = json.dumps(data).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self) -> None:
            assert self.headers["Authorization"] == "Bearer test-token"
            length = int(self.headers["Content-Length"] or 0)
            body = json.loads(self.rfile.read(length)) if length else {}
            if self.path == "/v1/machines":
                captured["create"] = body
                self.send_json(
                    200, {"id": "mach-cloud-1", "name": "harness", "state": "created"}
                )
            elif self.path == "/v1/machines/mach-cloud-1/start":
                self.send_json(200, {"id": "mach-cloud-1", "state": "started"})
            elif self.path == "/v1/machines/mach-cloud-1/exec/stream":
                captured.setdefault("commands", []).append(body)
                content = b'event: stdout\ndata: hello\n\nevent: exit\ndata: {"exitCode":0}\n\n'
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(content)))
                self.end_headers()
                self.wfile.write(content)
            else:
                self.send_json(404, {"message": "no route"})

        def do_GET(self) -> None:
            assert self.headers["Authorization"] == "Bearer test-token"
            self.send_json(
                200,
                {
                    "id": "mach-cloud-1",
                    "name": "harness",
                    "state": "started",
                    "ready": True,
                },
            )

        def do_DELETE(self) -> None:
            assert self.headers["Authorization"] == "Bearer test-token"
            captured["delete"] = self.path
            self.send_json(200, {"id": "mach-cloud-1"})

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server_thread = threading.Thread(target=server.serve_forever)
    server_thread.start()
    sandbox = SmolSandbox(
        target="cloud",
        base_url=f"http://127.0.0.1:{server.server_port}",
        api_key="test-token",
    )
    try:
        backend = SmolSandboxBackend(
            target="cloud", base_url=sandbox.base_url, api_key="test-token"
        )
        result = await backend.run(["echo", "hello"], env={"RUN": "yes"}, timeout=3)
        assert result.exit_code == 0 and result.stdout == "hello"
        ref = backend.ref
        assert ref == WorkspaceRef(provider="smol", id="mach-cloud-1")
        assert ref is not None
        assert captured["create"]["ttlSeconds"] == 3600
        assert captured["create"]["source"] == {
            "type": "image",
            "reference": "alpine:3.20",
        }
        assert captured["commands"][0]["env"] == {"RUN": "yes"}
        assert captured["commands"][0]["timeoutSeconds"] == 4
        attached = sandbox.backend(ref)
        assert (await attached.run(["echo", "hello"])).stdout == "hello"
        await sandbox.destroy(ref)
        assert captured["delete"] == "/v1/machines/mach-cloud-1"
    finally:
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=5)
