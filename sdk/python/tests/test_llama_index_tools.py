import asyncio
import importlib.util
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))

from smol.llama_index import SmolLlamaIndexTools, _MAX_OUTPUT


class FakeStream:
    def __init__(self, events):
        self.events = iter(events)
        self.killed = False

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.killed = True

    def __iter__(self):
        return self

    def __next__(self):
        return next(self.events)

    def kill(self):
        self.killed = True


class FakeMachine:
    def __init__(self, events=None):
        self.events = events or [
            {"kind": "stdout", "data": "hello\n"},
            {"kind": "exit", "exit_code": 0},
        ]
        self.executed = []
        self.deleted = False
        self.streams = []

    def exec_stream(self, command, opts):
        self.executed.append((command, opts))
        stream = FakeStream(self.events)
        self.streams.append(stream)
        return stream

    def delete(self):
        self.deleted = True


class SmolLlamaIndexToolsTest(unittest.TestCase):
    def test_owned_vm_is_reused_and_deleted_with_closed_egress(self):
        machine = FakeMachine()
        with patch("smol.llama_index.Machine.create", return_value=machine) as create:
            with SmolLlamaIndexTools() as sandbox:
                self.assertIn("hello", sandbox.run_command("echo hello"))
                self.assertIn("exit_code: 0", sandbox.run_python("print('hello')"))
            sandbox.close()
        self.assertEqual(create.call_count, 1)
        config, connection = create.call_args.args
        self.assertEqual(config.image, "python:3.12-alpine")
        self.assertFalse(config.resources.network)
        self.assertEqual(connection.target, "local")
        self.assertEqual(machine.executed[0][0], ["sh", "-c", "echo hello"])
        self.assertEqual(machine.executed[1][0], ["python3", "-c", "print('hello')"])
        self.assertEqual(machine.executed[0][1].workdir, "/workspace")
        self.assertTrue(machine.deleted)
        with self.assertRaisesRegex(RuntimeError, "closed"):
            sandbox.run_command("true")

    def test_caller_owned_vm_survives_context_and_failed_command_reports_status(self):
        machine = FakeMachine(
            [{"kind": "stderr", "data": "failed"}, {"kind": "exit", "exit_code": 7}]
        )
        with SmolLlamaIndexTools(machine=machine) as sandbox:
            output = sandbox.run_command("exit 7")
        self.assertIn("stderr:\nfailed", output)
        self.assertIn("exit_code: 7", output)
        self.assertFalse(machine.deleted)

    def test_large_output_stops_command_without_collecting_the_rest(self):
        machine = FakeMachine(
            [
                {"kind": "stdout", "data": "x" * (_MAX_OUTPUT + 2)},
                {"kind": "stdout", "data": "should not be read"},
            ]
        )
        with SmolLlamaIndexTools(machine=machine) as sandbox:
            output = sandbox.run_command("large")
        self.assertEqual(output.count("x"), _MAX_OUTPUT)
        self.assertNotIn("should not be read", output)
        self.assertIn("Output limit reached; output stream closed", output)
        self.assertTrue(machine.streams[0].killed)

    def test_output_cap_counts_utf8_bytes(self):
        machine = FakeMachine([{"kind": "stdout", "data": "🌍" * (_MAX_OUTPUT // 2)}])
        with SmolLlamaIndexTools(machine=machine) as sandbox:
            output = sandbox.run_python("print('unicode')")
        self.assertLessEqual(
            len(
                output.split("stdout:\n", 1)[1].split("\nstderr:", 1)[0].encode("utf-8")
            ),
            _MAX_OUTPUT,
        )
        self.assertIn("Output limit reached", output)
        self.assertTrue(machine.streams[0].killed)

    def test_cloud_has_bounded_ttl_and_invalid_config_is_rejected(self):
        machine = FakeMachine()
        with patch("smol.llama_index.Machine.create", return_value=machine) as create:
            with SmolLlamaIndexTools(target="cloud") as sandbox:
                sandbox.run_command("true")
        config, connection = create.call_args.args
        self.assertEqual(config.ttl_seconds, 3600)
        self.assertEqual(connection.target, "cloud")
        with self.assertRaises(ValueError):
            SmolLlamaIndexTools(target="unknown")
        with self.assertRaises(ValueError):
            SmolLlamaIndexTools(target="cloud", timeout_seconds=0)

    @unittest.skipUnless(
        importlib.util.find_spec("llama_index"),
        "install llama-index-core to check real tool contracts",
    )
    def test_function_tools_support_sync_and_async_calls(self):
        machine = FakeMachine()
        with SmolLlamaIndexTools(machine=machine) as sandbox:
            command, python = sandbox.tools()
            self.assertEqual(command.metadata.name, "smol_run_command")
            self.assertEqual(list(command.metadata.fn_schema.model_fields), ["command"])
            self.assertIn("exit_code: 0", command.call(command="echo hello").content)
            self.assertIn(
                "exit_code: 0", asyncio.run(python.acall(code="print('hello')")).content
            )
        self.assertFalse(machine.deleted)


if __name__ == "__main__":
    unittest.main()
