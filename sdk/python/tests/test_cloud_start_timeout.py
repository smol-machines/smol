"""Cloud starts get a long request deadline without relaxing other API calls."""

import sys
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))

from smol import ConnectOptions, Machine, MachineConfig, transport  # noqa: E402


def test_create_uses_long_timeout_for_cloud_start() -> None:
    for forkable in (False, True):
        starts: list[tuple[str, float]] = []

        def fetch(base_url, api_key, method, path, **kwargs):
            if method == "POST" and path == "/v1/machines":
                return {"id": "mach-large", "name": "large-image"}
            if method == "POST" and path.startswith("/v1/machines/mach-large/start"):
                starts.append((path, kwargs.get("timeout", transport.CLOUD_TIMEOUT_S)))
                return {"id": "mach-large", "state": "started"}
            raise AssertionError(f"unexpected request: {method} {path}")

        with (
            patch.object(transport, "_cloud_fetch", fetch),
            patch.object(transport, "_wait_for_ready", lambda *args, **kwargs: None),
        ):
            machine = Machine.create(
                MachineConfig(
                    image="example.invalid/large:latest",
                    forkable=forkable,
                    ready_timeout_seconds=5,
                ),
                ConnectOptions(
                    target="cloud", api_key="smk_test", base_url="http://cloud"
                ),
            )

        assert machine.name == "large-image"
        assert starts == [
            (
                "/v1/machines/mach-large/start"
                + ("?forkable=true" if forkable else ""),
                transport.CLOUD_START_TIMEOUT_S,
            )
        ]


def test_resume_uses_long_timeout_for_cloud_start() -> None:
    starts: list[float] = []

    def fetch(base_url, api_key, method, path, **kwargs):
        assert method == "POST"
        assert path == "/v1/machines/mach-large/start"
        starts.append(kwargs.get("timeout", transport.CLOUD_TIMEOUT_S))
        return {"id": "mach-large", "state": "started"}

    with (
        patch.object(transport, "_cloud_fetch", fetch),
        patch.object(transport, "_wait_for_ready", lambda *args, **kwargs: None),
    ):
        cloud = transport.CloudTransport(
            "http://cloud", "smk_test", "mach-large", "large-image"
        )
        cloud.start()

    assert starts == [transport.CLOUD_START_TIMEOUT_S]


def main() -> int:
    test_create_uses_long_timeout_for_cloud_start()
    test_resume_uses_long_timeout_for_cloud_start()
    print("RESULT=PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
