"""Real cloud e2e against a LIVE smolfleet /v1 server (localhost).

Driven by SMOL_CLOUD_URL + SMOL_CLOUD_TOKEN (a real smk_ key). Mirrors the Node
`test/cloud-real.ts`.
"""

import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))

from smol import ConnectOptions, Machine, MachineConfig, ResourceSpec  # noqa: E402


def main() -> int:
    base_url = os.environ["SMOL_CLOUD_URL"]
    api_key = os.environ["SMOL_CLOUD_TOKEN"]
    image = os.environ.get("SMOL_TEST_IMAGE", "alpine")
    print(f"target={base_url} key={api_key[:8]}… image={image}")

    m = Machine.create(
        MachineConfig(image=image, resources=ResourceSpec(cpus=1, memory_mb=512, network=True)),
        ConnectOptions(target="cloud", base_url=base_url, api_key=api_key),
    )
    print("CREATED", m.name, "state=", m.state())
    try:
        r = m.exec(["sh", "-c", "echo cloud-real-ok && uname -sm"])
        print("EXEC stdout=", repr(r.stdout), "exit=", r.exit_code)
        m.write_file("/tmp/z", "realrt")
        print("FILE=", m.read_file("/tmp/z").decode())
        ok = r.exit_code == 0 and "cloud-real-ok" in r.stdout
        print("CLOUD_REAL_OK" if ok else "CLOUD_REAL_PARTIAL")
        return 0 if ok else 2
    finally:
        m.delete()
        print("deleted")


if __name__ == "__main__":
    sys.exit(main())
