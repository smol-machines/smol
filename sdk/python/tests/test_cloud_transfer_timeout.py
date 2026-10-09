"""Cloud file transfers get time in proportion to their size."""

import sys
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))

from smol import ConnectOptions, Machine, MachineConfig, transport  # noqa: E402


def test_a_large_upload_is_not_cut_off_at_the_request_timeout() -> None:
    sent: list[tuple[str, str, float]] = []

    def fetch(base_url, api_key, method, path, **kwargs):
        if method == "POST" and path == "/v1/machines":
            return {"id": "mach-files", "name": "files"}
        if method == "POST" and path.startswith("/v1/machines/mach-files/start"):
            return {"id": "mach-files", "state": "started"}
        if "/files/" in path:
            sent.append((method, path, kwargs.get("timeout", transport.CLOUD_TIMEOUT_S)))
            return b"" if method == "GET" else None
        raise AssertionError(f"unexpected request: {method} {path}")

    with (
        patch.object(transport, "_cloud_fetch", fetch),
        patch.object(transport, "_wait_for_ready", lambda *args, **kwargs: None),
    ):
        machine = Machine.create(
            MachineConfig(image="alpine"),
            ConnectOptions(target="cloud", api_key="smk_test", base_url="http://cloud"),
        )
        machine.write_file("/tmp/small", b"x")
        bundle = b"\0" * (64 * 1024 * 1024)
        machine.write_file("/tmp/repo.bundle", bundle)
        machine.read_file("/tmp/report.json")

    (_, _, small), (_, _, large), (_, _, read) = sent
    assert small == transport.CLOUD_TIMEOUT_S + 1
    # 64 MiB at the 256 KiB/s floor is 256 s on top of the request timeout.
    assert large == transport.CLOUD_TIMEOUT_S + 256
    assert read == transport.CLOUD_TIMEOUT_S + 400


def test_a_file_over_the_cloud_limit_is_refused_before_it_is_sent() -> None:
    sent: list[str] = []

    def fetch(base_url, api_key, method, path, **kwargs):
        if method == "POST" and path == "/v1/machines":
            return {"id": "mach-files", "name": "files"}
        if method == "POST" and path.startswith("/v1/machines/mach-files/start"):
            return {"id": "mach-files", "state": "started"}
        sent.append(path)
        return None

    with (
        patch.object(transport, "_cloud_fetch", fetch),
        patch.object(transport, "_wait_for_ready", lambda *args, **kwargs: None),
    ):
        machine = Machine.create(
            MachineConfig(image="alpine"),
            ConnectOptions(target="cloud", api_key="smk_test", base_url="http://cloud"),
        )
        try:
            machine.write_file("/tmp/big.bin", b"\0" * (transport.CLOUD_MAX_FILE_BYTES + 1))
        except transport.SmolError as e:
            assert "accept files up to 100 MiB" in str(e)
        else:
            raise AssertionError("an oversized upload was not refused")

    assert not sent, f"an oversized upload still sent {sent}"
