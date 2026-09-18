"""Client contract tests; real-VM growth is a separate acceptance gate."""
import asyncio
import unittest
from unittest.mock import Mock

from smol import AsyncMachine, Machine, ResizeOptions
from smol.errors import SmolError
from smol.transport import LocalTransport


class ResizeTests(unittest.TestCase):
    def test_invalid_targets(self):
        for values in ({}, {"cpus": 0}, {"cpus": True}, {"cpus": 1.5},
                       {"cpus": 256}, {"memory_mb": 2**32}, {"storage_gb": 2**34},
                       {"overlay_gb": -1}, {"cpus": 4, "memory_mb": 2048}):
            with self.subTest(values=values), self.assertRaises(ValueError):
                ResizeOptions(**values)

    def test_machine_forwards_absolute_targets(self):
        transport = Mock()
        options = ResizeOptions(storage_gb=4, overlay_gb=8)
        Machine(transport).resize(options)
        transport.resize.assert_called_once_with(options)

    def test_local_transport_does_not_restart(self):
        transport = object.__new__(LocalTransport)
        transport._inner = Mock()
        transport.resize(ResizeOptions(memory_mb=2048))
        transport._inner.resize.assert_called_once_with(
            cpus=None, memory_mb=2048, storage_gb=None, overlay_gb=None)
        transport._inner.start.assert_not_called()
        transport._inner.stop.assert_not_called()

    def test_local_error_propagates_without_retry_or_restart(self):
        transport = object.__new__(LocalTransport)
        transport._inner = Mock()
        transport._inner.resize.side_effect = RuntimeError("[CONFLICT] pending resize")
        with self.assertRaises(SmolError) as caught:
            transport.resize(ResizeOptions(cpus=4))
        self.assertEqual(caught.exception.code, "CONFLICT")
        self.assertEqual(transport._inner.resize.call_count, 1)
        transport._inner.start.assert_not_called()

    def test_async_forwarding(self):
        machine = Mock()
        options = ResizeOptions(cpus=4)
        asyncio.run(AsyncMachine(machine).resize(options))
        machine.resize.assert_called_once_with(options)


if __name__ == "__main__":
    unittest.main()
