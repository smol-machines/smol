"""Async-transport test: AsyncMachine against a threaded mock of smolfleet /v1.

Verifies the awaitable API mirrors the sync one AND that calls don't block the
event loop — several machines are created/driven concurrently with
``asyncio.gather`` and their slow (sleepy) boots overlap in wall-clock time.

    python tests/test_async_mock.py
"""

from __future__ import annotations

import asyncio
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from smol import AsyncMachine, ConnectOptions, MachineConfig, PortSpec

# Each machine's GET returns not-ready for the first BOOT_DELAY_S, then ready.
BOOT_DELAY_S = 0.4
_created_at: dict[str, float] = {}
captured: dict = {"connect_paths": []}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):  # silence
        pass

    def _auth_ok(self) -> bool:
        return self.headers.get("authorization") == "Bearer smk_async"

    def _send(self, code: int, body: bytes = b"", ctype: str = "application/json"):
        self.send_response(code)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)

    def _read(self) -> bytes:
        n = int(self.headers.get("content-length", 0) or 0)
        return self.rfile.read(n) if n else b""

    def do_POST(self):
        if not self._auth_ok():
            return self._send(401, b"bad token")
        if self.path == "/v1/machines":
            body = json.loads(self._read() or b"{}")
            mid = body.get("name") or "auto"
            _created_at[mid] = time.monotonic()
            return self._send(200, json.dumps({"id": mid, "name": mid, "state": "created"}).encode())
        # start / stop / exec
        parts = self.path.split("/")
        mid = parts[3] if len(parts) > 3 else ""
        if self.path.endswith("/exec"):
            self._read()
            return self._send(200, json.dumps({"exitCode": 0, "stdout": f"{mid}-ok\n", "stderr": ""}).encode())
        return self._send(200, json.dumps({"id": mid, "state": "started"}).encode())

    def do_GET(self):
        if not self._auth_ok():
            return self._send(401, b"bad token")
        parts = self.path.split("/")
        mid = parts[3] if len(parts) > 3 else ""
        if "/connect/" in self.path:
            captured["connect_paths"].append(self.path)
            return self._send(200, json.dumps({"ok": True, "path": self.path}).encode())
        # Readiness flips true only after BOOT_DELAY_S — mimics a real boot so a
        # blocking wait would serialize; a non-blocking one overlaps.
        started = _created_at.get(mid, 0.0)
        ready = (time.monotonic() - started) >= BOOT_DELAY_S if started else True
        payload = {"id": mid, "state": "started", "ready": ready}
        if ready:
            payload["readyAt"] = "2026-07-22T20:01:41.152Z"
        return self._send(200, json.dumps(payload).encode())

    def do_DELETE(self):
        if not self._auth_ok():
            return self._send(401, b"bad token")
        return self._send(204)


async def run() -> int:
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    port = server.server_address[1]
    import threading

    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{port}"
    conn = ConnectOptions(target="cloud", base_url=base, api_key="smk_async")

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
        # --- single machine: awaitable surface mirrors the sync one ---
        m = await AsyncMachine.create(
            MachineConfig(image="alpine:3.20", name="solo",
                          ports=[PortSpec(host=8080, guest=8080)]),
            conn,
        )
        check("create returns an AsyncMachine", isinstance(m, AsyncMachine))
        check("state() awaitable", (await m.state()) == "started")
        check("ready() awaitable + true after boot", (await m.ready()) is True)
        check("ready_at() awaitable", (await m.ready_at()) == "2026-07-22T20:01:41.152Z")
        r = await m.exec(["echo", "hi"])
        check("exec() awaitable", r.stdout.strip() == "solo-ok")
        ep = m.endpoint(8080, "/socket")
        check("endpoint() builds authed ws url (sync, no I/O)",
              ep.ws_url == f"{base.replace('http', 'ws', 1)}/v1/machines/solo/connect/8080/socket"
              and ep.headers.get("authorization") == "Bearer smk_async", ep.ws_url)
        body = json.loads((await m.request(8080, "healthz")).decode())
        check("request() awaitable reaches the bridge", body.get("ok") is True)
        await m.delete()

        # --- concurrency: N sleepy boots overlap, proving non-blocking ---
        n = 5
        t0 = time.monotonic()
        machines = await asyncio.gather(
            *(AsyncMachine.create(MachineConfig(image="alpine:3.20", name=f"w{i}"), conn) for i in range(n))
        )
        elapsed = time.monotonic() - t0
        check(f"created {n} machines concurrently", len(machines) == n)
        # If create() blocked the loop, wall-clock would be ~n * BOOT_DELAY_S.
        # Non-blocking, the boots overlap and it lands well under that.
        check("concurrent creates overlapped (non-blocking)",
              elapsed < BOOT_DELAY_S * n * 0.6,
              f"{elapsed:.2f}s vs serialized ~{BOOT_DELAY_S * n:.2f}s")
        outs = await asyncio.gather(*(mm.exec(["echo", "hi"]) for mm in machines))
        check("concurrent exec on all", all(o.success for o in outs))
        await asyncio.gather(*(mm.delete() for mm in machines))

        # --- async context manager ---
        async with await AsyncMachine.create(MachineConfig(image="alpine:3.20", name="ctx"), conn) as cm:
            check("async with yields a usable machine", (await cm.ready()) is True)
    finally:
        server.shutdown()

    print(f"\n{passed} passed, {failed} failed")
    return 1 if failed else 0


def test_async_mock():
    assert asyncio.run(run()) == 0


if __name__ == "__main__":
    sys.exit(asyncio.run(run()))
