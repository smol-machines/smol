"""Live regression for attaching to durable saved execution and resuming it."""

import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))

from smol import ConnectOptions, Machine, MachineConfig  # noqa: E402


def main() -> None:
    name = f"py-paused-connect-{time.time_ns()}"
    conn = ConnectOptions(target="local")
    machine = Machine.create(MachineConfig(name=name, persistent=True, network=False), conn)
    try:
        machine.exec(["sh", "-c", "echo persisted >/tmp/paused-connect.txt"]).assert_success()
        machine.pause()
        assert machine.state() == "paused"
        attached = Machine.connect(name, conn)
        assert attached.state() == "paused"
        assert not attached.ready()
        attached.resume()
        assert attached.state() == "running"
        assert attached.exec(["cat", "/tmp/paused-connect.txt"]).stdout.strip() == "persisted"
    finally:
        machine.delete()


if __name__ == "__main__":
    main()
