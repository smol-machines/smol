"""Per-command user, killable streams, timeouts and host checks — pure Python.

The cloud half runs against a loopback mock of the control plane; the local
half against a fake native module. No VM, no real network.
"""

import json
import os
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))

from smol import (  # noqa: E402
    ConnectOptions,
    ExecOptions,
    ExecStream,
    InvalidConfigError,
    Machine,
    MachineConfig,
    NotSupportedError,
    ResourceSpec,
)
from smol import transport as transport_module  # noqa: E402
from smol.transport import CloudTransport, LocalTransport  # noqa: E402

KEY = "smk_testkey"
MID = "mach-x"


class Control:
    """What the mock control plane does and saw."""

    def __init__(self) -> None:
        self.echo_user = True
        self.exec_bodies: list[dict] = []
        self.stream_bodies: list[dict] = []
        self.create_bodies: list[dict] = []
        self.stream_mode = "exit"  # "exit" | "hold" | "quiet"
        self.stream_closed = threading.Event()


control = Control()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):  # silence
        pass

    def _json(self, code: int, body: dict) -> None:
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _body(self) -> dict:
        n = int(self.headers.get("content-length", 0))
        return json.loads(self.rfile.read(n) or b"{}") if n else {}

    def do_GET(self):
        self._json(200, {"id": MID, "name": "m", "state": "running", "ready": True, "ports": []})

    def do_POST(self):
        if self.path == "/v1/machines":
            control.create_bodies.append(self._body())
            return self._json(201, {"id": MID, "name": "m", "state": "stopped"})
        if self.path.startswith(f"/v1/machines/{MID}/start"):
            return self._json(200, {"id": MID, "state": "started"})
        if self.path == f"/v1/machines/{MID}/exec":
            body = self._body()
            control.exec_bodies.append(body)
            reply = {"exitCode": 0, "stdout": "ok", "stderr": ""}
            if control.echo_user and "user" in body:
                reply["user"] = body["user"]
            return self._json(200, reply)
        if self.path == f"/v1/machines/{MID}/exec/stream":
            control.stream_bodies.append(self._body())
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("connection", "close")
            self.end_headers()
            if control.stream_mode == "exit":
                self.wfile.write(b"event: stdout\ndata: hi\n\nevent: exit\ndata: {\"exitCode\":0}\n\n")
            elif control.stream_mode == "quiet":
                self.wfile.write(b"event: stdout\ndata: working\n\n")
                self.wfile.flush()
                time.sleep(2.5)
                self.wfile.write(b"event: exit\ndata: {\"exitCode\":0}\n\n")
            else:  # hold until the client hangs up
                self.wfile.write(b"event: stdout\ndata: started\n\n")
                self.wfile.flush()
                self.connection.settimeout(10)
                try:
                    while self.connection.recv(64):
                        pass
                except OSError:
                    pass
                control.stream_closed.set()
            self.close_connection = True
            return
        self._json(404, {"error": "no route"})


def cloud(base: str) -> CloudTransport:
    return CloudTransport(base, KEY, MID, "m")


# --------------------------------------------------------------------------- cloud


def test_user_is_proven_once_then_sent_with_each_command(base):
    control.__init__()
    t = cloud(base)
    t.exec(["id"], ExecOptions(user="nobody"))
    t.exec(["id"], ExecOptions(user="nobody"))
    t.exec(["id"])
    commands = [b["command"][0] for b in control.exec_bodies]
    assert commands == ["true", "id", "id", "id"], commands
    assert [b.get("user") for b in control.exec_bodies] == ["nobody", "nobody", "nobody", None]


def test_old_control_plane_runs_nothing_as_anyone(base):
    control.__init__()
    control.echo_user = False
    t = cloud(base)
    for call in (
        lambda: t.exec(["rm", "-rf", "/data"], ExecOptions(user="nobody")),
        lambda: t.exec_stream(["rm", "-rf", "/data"], ExecOptions(user="nobody")),
    ):
        try:
            call()
        except NotSupportedError as e:
            assert "nobody" in str(e)
        else:
            raise AssertionError("an ignored user must be refused")
    # Only probes reached the machine; the real command never ran.
    assert all(b["command"] == ["true"] for b in control.exec_bodies), control.exec_bodies
    assert control.stream_bodies == []


def test_timeouts_round_up_and_zero_is_refused(base):
    control.__init__()
    t = cloud(base)
    t.exec(["true"], ExecOptions(timeout=1.5))
    t.exec(["true"], ExecOptions(timeout=0.2))
    for bad in (0, -1):
        try:
            t.exec(["true"], ExecOptions(timeout=bad))
        except InvalidConfigError:
            pass
        else:
            raise AssertionError(f"timeout={bad} must be refused")
    assert [b["timeoutSeconds"] for b in control.exec_bodies] == [2, 1]


def test_a_killed_stream_closes_its_connection_while_the_reader_waits(base):
    control.__init__()
    control.stream_mode = "hold"
    stream = cloud(base).exec_stream(["sleep", "infinity"])
    assert next(stream) == {"kind": "stdout", "data": "started"}
    threading.Timer(0.2, stream.kill).start()
    started = time.monotonic()
    assert list(stream) == [], "a killed stream ends without an error"
    assert time.monotonic() - started < 5
    assert control.stream_closed.wait(5), "the connection closes at once"


def test_a_quiet_stream_outlives_the_request_timeout(base):
    control.__init__()
    control.stream_mode = "quiet"
    # The request timeout used to apply to every read, so a command that went
    # quiet for longer than it failed mid-stream.
    with mock.patch.object(transport_module, "CLOUD_TIMEOUT_S", 1.0):
        events = list(cloud(base).exec_stream(["make"]))
    assert events == [
        {"kind": "stdout", "data": "working"},
        {"kind": "exit", "exit_code": 0},
    ], events


def test_network_off_is_blocked_and_a_machine_user_is_refused(base):
    control.__init__()
    conn = ConnectOptions(target="cloud", base_url=base, api_key=KEY)
    Machine.create(MachineConfig(image="alpine", resources=ResourceSpec(network=False)), conn)
    Machine.create(MachineConfig(image="alpine", network=True), conn)
    Machine.create(MachineConfig(image="alpine"), conn)
    networks = [b.get("network") for b in control.create_bodies]
    assert networks == [{"mode": "blocked"}, {"mode": "open"}, None], networks
    try:
        Machine.create(MachineConfig(image="alpine", user="nobody"), conn)
    except NotSupportedError:
        pass
    else:
        raise AssertionError("the API has no machine-wide user")
    assert len(control.create_bodies) == 3


# --------------------------------------------------------------------------- local


class FakeNativeStream:
    def __init__(self, events):
        self.events = list(events)
        self.killed = 0

    def __iter__(self):
        return self

    def __next__(self):
        if not self.events:
            raise StopIteration
        return self.events.pop(0)

    def kill(self):
        self.killed += 1


class FakeNative:
    def __init__(self):
        self.calls = []
        self.stream = FakeNativeStream(
            [{"kind": "stdout", "data": "a"}, {"kind": "stdout", "data": "b"}, {"kind": "exit", "exit_code": 0}]
        )

    def exec(self, command, options, *cancel):
        self.calls.append(("exec", command, options, cancel))
        return SimpleNamespace(exit_code=0, stdout="", stderr="")

    def run(self, image, command, options):
        self.calls.append(("run", image, command, options))
        return SimpleNamespace(exit_code=0, stdout="", stderr="")

    def exec_stream(self, command, options):
        self.calls.append(("exec_stream", command, options))
        return self.stream


def local(native) -> LocalTransport:
    return LocalTransport(native, cleanup_on_exit=False)


def test_local_options_carry_user_and_millisecond_timeouts():
    native = FakeNative()
    t = local(native)
    t.exec(["id"], ExecOptions(user="nobody", timeout=1.5))
    t.exec(["id"], ExecOptions(timeout=0.0001))
    assert native.calls[0][2] == {"timeout_ms": 1500, "user": "nobody"}, native.calls[0]
    assert native.calls[1][2] == {"timeout_ms": 1}, native.calls[1]
    for bad in (ExecOptions(timeout=0), ExecOptions(user="  ")):
        try:
            t.exec(["id"], bad)
        except InvalidConfigError:
            pass
        else:
            raise AssertionError(f"{bad} must be refused")
    try:
        t.run("alpine", ["id"], ExecOptions(user="nobody"))
    except NotSupportedError:
        pass
    else:
        raise AssertionError("run(image) cannot honor a user")
    assert [c[0] for c in native.calls] == ["exec", "exec"]


def test_breaking_out_of_a_stream_kills_the_command():
    native = FakeNative()
    for event in local(native).exec_stream(["tail", "-f", "log"]):
        break
    # Dropping the stream after `break` kills the command it abandoned.
    assert native.stream.killed == 1


def test_a_finished_stream_is_not_killed():
    native = FakeNative()
    events = list(local(native).exec_stream(["true"]))
    assert events[-1] == {"kind": "exit", "exit_code": 0}
    assert native.stream.killed == 0


def test_kill_ends_the_stream_once_and_hides_the_teardown():
    kills = []
    stream = ExecStream(iter([{"kind": "stdout", "data": "a"}, {"kind": "exit", "exit_code": -9}]), lambda: kills.append(1))
    assert next(stream) == {"kind": "stdout", "data": "a"}
    stream.kill()
    stream.kill()
    with stream:
        pass
    assert list(stream) == []
    assert kills == [1]

    def broken():
        yield {"kind": "stdout", "data": "a"}
        raise OSError("connection reset")

    stream = ExecStream(broken(), lambda: None)
    next(stream)
    stream.kill()
    assert list(stream) == [], "an error caused by the kill is not reported"


def test_importing_smol_leaves_the_environment_alone():
    env = {k: v for k, v in os.environ.items() if not k.startswith("SMOLVM_")}
    code = (
        "import os, smol\n"
        "print(sorted(k for k in os.environ if k.startswith('SMOLVM_')))\n"
    )
    out = subprocess.run(
        [sys.executable, "-c", code],
        env={**env, "PYTHONPATH": str(Path(__file__).resolve().parents[1] / "python")},
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    assert out == "[]", out


def test_local_availability_reports_an_unsupported_platform_without_loading_anything():
    with mock.patch.object(transport_module.platform, "machine", return_value="riscv64"), \
            mock.patch.object(transport_module, "_load_native", side_effect=AssertionError("loaded")):
        available, code, reason = transport_module.local_availability()
    assert (available, code) == (False, "UNSUPPORTED_PLATFORM"), (available, code, reason)


def test_local_availability_passes_the_engine_verdict_through():
    native = SimpleNamespace(Machine=SimpleNamespace(check_host=lambda: (False, "KVM_UNAVAILABLE", "no /dev/kvm")))
    with mock.patch.object(transport_module.sys, "platform", "linux"), \
            mock.patch.object(transport_module.platform, "machine", return_value="x86_64"), \
            mock.patch.object(transport_module.platform, "libc_ver", return_value=("glibc", "2.39")), \
            mock.patch.object(transport_module, "_load_native", return_value=native):
        assert transport_module.local_availability() == (False, "KVM_UNAVAILABLE", "no /dev/kvm")
    native.Machine.check_host = lambda: (True, None, None)
    with mock.patch.object(transport_module.sys, "platform", "linux"), \
            mock.patch.object(transport_module.platform, "machine", return_value="x86_64"), \
            mock.patch.object(transport_module.platform, "libc_ver", return_value=("glibc", "2.28")), \
            mock.patch.object(transport_module, "_load_native", return_value=native):
        available, code, _ = transport_module.local_availability()
    assert (available, code) == (False, "UNSUPPORTED_PLATFORM")


def main() -> int:
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{server.server_address[1]}"
    failed = 0
    tests = [(k, v) for k, v in globals().items() if k.startswith("test_") and callable(v)]
    for name, fn in tests:
        try:
            fn(base) if fn.__code__.co_argcount else fn()
            print(f"  ok {name}")
        except Exception as e:  # noqa: BLE001
            failed += 1
            print(f"  FAIL {name}: {type(e).__name__}: {e}")
    server.shutdown()
    print(f"\n{len(tests) - failed} passed, {failed} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
