"""Explicit real-VM acceptance: run this file on Linux x86_64 with native SDK.

Requires KVM, built native extension and configured engine runtime assets.
Not collected as a mock/unit test; missing prerequisites fail rather than skip.
"""
import platform

from smol import ConnectOptions, Machine, MachineConfig, ResizeOptions, ResourceSpec
from smol.errors import SmolError


def main():
    assert platform.system() == "Linux"
    assert platform.machine() == "x86_64"
    machine = Machine.create(
        MachineConfig(resources=ResourceSpec(
            cpus=2, memory_mb=512, storage_gb=1, overlay_gb=1)),
        ConnectOptions(target="local"),
    )

    def guest(command):
        result = machine.exec(["sh", "-ec", command])
        result.assert_success()
        return result.stdout.strip()

    try:
        boot = guest("cat /proc/sys/kernel/random/boot_id")
        guest("echo alive >/dev/shm/resize-marker; echo disk >/storage/resize-marker")
        machine.resize(ResizeOptions(cpus=4))
        assert guest("taskset -c 3 echo cpu-active") == "cpu-active"
        machine.resize(ResizeOptions(memory_mb=1024))
        guest("mount -o remount,size=800M /dev/shm; "
              "dd if=/dev/zero of=/dev/shm/grown bs=1M count=704; "
              "test $(stat -c %s /dev/shm/grown) = 738197504")
        machine.resize(ResizeOptions(storage_gb=2, overlay_gb=2))
        machine.resize(ResizeOptions(storage_gb=2, overlay_gb=2))
        try:
            machine.resize(ResizeOptions(storage_gb=1))
        except SmolError:
            pass
        else:
            raise AssertionError("disk shrink unexpectedly succeeded")
        assert guest("cat /proc/sys/kernel/random/boot_id") == boot
        guest("test $(cat /dev/shm/resize-marker) = alive; "
              "test $(cat /storage/resize-marker) = disk; "
              "test $(df -k /storage | tail -1 | awk '{print $2}') -gt 1900000; "
              "test $(df -k / | tail -1 | awk '{print $2}') -gt 1900000")
        print("Python local CPU/RAM/disk growth and boot continuity passed")
    finally:
        machine.delete()


if __name__ == "__main__":
    main()
