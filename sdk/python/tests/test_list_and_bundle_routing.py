"""Bundled engine paths stay in-process so child processes do not inherit them."""

import os
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))

from smol import transport as transport_module  # noqa: E402


class FakeNative:
    def __init__(self):
        self.bundle_calls = []

    def configure_bundle(self, **kw):
        self.bundle_calls.append(kw)

class BundleWiring(unittest.TestCase):
    def test_bundle_goes_to_the_engine_not_the_environment(self):
        native = FakeNative()
        with mock.patch.dict(os.environ, {}, clear=False):
            for k in ("SMOLVM_BOOT_BINARY", "SMOLVM_LIB_DIR", "SMOLVM_AGENT_ROOTFS_TAR"):
                os.environ.pop(k, None)
            with mock.patch.object(transport_module, "_bundled_paths", return_value=("/pkg/smol-vmm", "/pkg", "/pkg/agent-rootfs.tar")):
                transport_module._wire_bundled_native(native)
            self.assertEqual(native.bundle_calls, [{"boot_binary": "/pkg/smol-vmm", "lib_dir": "/pkg", "agent_rootfs_tar": "/pkg/agent-rootfs.tar"}])
            for k in ("SMOLVM_BOOT_BINARY", "SMOLVM_LIB_DIR", "SMOLVM_AGENT_ROOTFS_TAR"):
                self.assertNotIn(k, os.environ, f"{k} leaked into the environment")

    def test_an_explicit_variable_still_wins(self):
        native = FakeNative()
        with mock.patch.dict(os.environ, {"SMOLVM_BOOT_BINARY": "/my/helper"}, clear=False):
            with mock.patch.object(transport_module, "_bundled_paths", return_value=("/pkg/smol-vmm", "/pkg", None)):
                transport_module._wire_bundled_native(native)
            self.assertEqual(native.bundle_calls[0]["boot_binary"], "/my/helper")

    def test_an_older_engine_without_the_call_falls_back_to_the_environment(self):
        class Old:
            pass
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("SMOLVM_BOOT_BINARY", None)
            with mock.patch.object(transport_module, "_bundled_paths", return_value=("/pkg/smol-vmm", "/pkg", None)):
                transport_module._wire_bundled_native(Old())
            self.assertEqual(os.environ.get("SMOLVM_BOOT_BINARY"), "/pkg/smol-vmm")


if __name__ == "__main__":
    unittest.main()
