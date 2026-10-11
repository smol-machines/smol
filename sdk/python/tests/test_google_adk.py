import sys
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))

try:
    from google.adk.code_executors.code_execution_utils import (
        CodeExecutionInput,
        File,
    )
    from smol.google_adk import _MAX_OUTPUT, SmolCodeExecutor
except ImportError:
    SmolCodeExecutor = None


def context(user: str, session: str = "same-id"):
    return SimpleNamespace(
        session=SimpleNamespace(app_name="agent", user_id=user, id=session)
    )


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
    def __init__(self, events=None, delete_error=False):
        self.events = events or [
            {"kind": "stdout", "data": "hello\n"},
            {"kind": "exit", "exit_code": 0},
        ]
        self.delete_error = delete_error
        self.deleted = False
        self.streams = []
        self.executed = []

    def exec_stream(self, cmd, opts):
        self.executed.append((cmd, opts))
        stream = FakeStream(self.events)
        self.streams.append(stream)
        return stream

    def delete(self):
        if self.delete_error:
            raise RuntimeError("delete failed")
        self.deleted = True


@unittest.skipIf(
    SmolCodeExecutor is None, "install smolmachines[google-adk] to test ADK integration"
)
class SmolCodeExecutorTest(unittest.TestCase):
    def test_adk_llm_agent_accepts_the_executor(self):
        from google.adk.agents import LlmAgent

        with SmolCodeExecutor() as executor:
            agent = LlmAgent(
                name="sandboxed_coder",
                model="gemini-2.5-flash",
                code_executor=executor,
            )
            self.assertIs(agent.code_executor, executor)

    def test_sessions_are_isolated_reused_and_deleted(self):
        first, second = FakeMachine(), FakeMachine()
        with patch(
            "smol.google_adk.Machine.create", side_effect=[first, second]
        ) as create:
            with SmolCodeExecutor() as executor:
                self.assertEqual(
                    executor.execute_code(
                        context("alice"), CodeExecutionInput(code="print(1)")
                    ).stdout,
                    "hello\n",
                )
                executor.execute_code(
                    context("alice"), CodeExecutionInput(code="print(2)")
                )
                executor.execute_code(
                    context("bob"), CodeExecutionInput(code="print(3)")
                )
            self.assertEqual(create.call_count, 2)
        self.assertTrue(first.deleted)
        self.assertTrue(second.deleted)
        self.assertEqual(len(first.executed), 2)
        self.assertEqual(first.executed[0][0], ["python3", "-c", "print(1)"])
        self.assertEqual(first.executed[0][1].workdir, "/workspace")
        config, connection = create.call_args.args
        self.assertFalse(config.resources.network)
        self.assertEqual(connection.target, "local")
        with self.assertRaisesRegex(RuntimeError, "closed"):
            executor.execute_code(context("alice"), CodeExecutionInput(code="print(4)"))

    def test_close_session_releases_only_its_vm(self):
        first, second = FakeMachine(), FakeMachine()
        with patch(
            "smol.google_adk.Machine.create", side_effect=[first, second]
        ) as create:
            with SmolCodeExecutor() as executor:
                executor.execute_code(
                    context("alice"), CodeExecutionInput(code="print(1)")
                )
                executor.execute_code(
                    context("bob"), CodeExecutionInput(code="print(2)")
                )
                executor.close_session("agent", "alice", "same-id")
                self.assertTrue(first.deleted)
                self.assertFalse(second.deleted)
                executor.execute_code(
                    context("bob"), CodeExecutionInput(code="print(3)")
                )
                self.assertEqual(create.call_count, 2)
            self.assertTrue(second.deleted)

    def test_nonzero_exit_and_successful_stderr_follow_adk_contract(self):
        failed = FakeMachine([{"kind": "exit", "exit_code": 17}])
        with (
            patch("smol.google_adk.Machine.create", return_value=failed),
            SmolCodeExecutor() as executor,
        ):
            result = executor.execute_code(
                context("alice"), CodeExecutionInput(code="raise SystemExit(17)")
            )
        self.assertEqual(result.exit_code, 17)
        self.assertIn("status 17", result.stderr)
        warning = FakeMachine(
            [{"kind": "stderr", "data": "warning"}, {"kind": "exit", "exit_code": 0}]
        )
        with (
            patch("smol.google_adk.Machine.create", return_value=warning),
            SmolCodeExecutor() as executor,
        ):
            self.assertEqual(
                executor.execute_code(
                    context("alice"), CodeExecutionInput(code="print(1)")
                ).stderr,
                "",
            )

    def test_output_is_bounded_in_bytes_and_stream_is_closed(self):
        machine = FakeMachine([{"kind": "stdout", "data": "🌍" * (_MAX_OUTPUT // 2)}])
        with (
            patch("smol.google_adk.Machine.create", return_value=machine),
            SmolCodeExecutor() as executor,
        ):
            result = executor.execute_code(
                context("alice"), CodeExecutionInput(code="print('large')")
            )
        self.assertLessEqual(len(result.stdout.encode("utf-8")), _MAX_OUTPUT)
        self.assertIn("Output limit reached", result.stderr)
        self.assertTrue(machine.streams[0].killed)

    def test_cloud_ttl_secrets_and_file_optimization_are_explicit(self):
        from smol import ConnectOptions

        machine = FakeMachine()
        with (
            patch("smol.google_adk.Machine.create", return_value=machine) as create,
            SmolCodeExecutor(
                target="cloud",
                connection=ConnectOptions(target="cloud", api_key="test-secret"),
            ) as executor,
        ):
            self.assertNotIn("test-secret", executor.model_dump_json())
            unsupported = executor.execute_code(
                context("alice"),
                CodeExecutionInput(
                    code="print(1)", input_files=[File("input.csv", b"data")]
                ),
            )
            self.assertEqual(unsupported.exit_code, 1)
            self.assertEqual(create.call_count, 0)
            executor.execute_code(context("alice"), CodeExecutionInput(code="print(1)"))
        config, connection = create.call_args.args
        self.assertEqual(config.ttl_seconds, 3600)
        self.assertEqual(connection.api_key, "test-secret")
        for option in (
            {"stateful": True},
            {"optimize_data_file": True},
            {"target": "unknown"},
            {"timeout_seconds": 0},
        ):
            with self.assertRaises(ValueError):
                SmolCodeExecutor(**option)

    def test_expired_cloud_vm_cleanup_and_oversized_source(self):
        from smol import SmolError

        class ExpiredMachine(FakeMachine):
            def delete(self):
                raise SmolError("NOT_FOUND", "expired")

        machine = ExpiredMachine()
        with patch("smol.google_adk.Machine.create", return_value=machine) as create:
            with SmolCodeExecutor(target="cloud") as executor:
                large = executor.execute_code(
                    context("alice"), CodeExecutionInput(code="🌍" * (64 * 1024 // 2))
                )
                self.assertEqual(large.exit_code, 1)
                self.assertIn("64 KiB", large.stderr)
                self.assertEqual(create.call_count, 0)
                executor.execute_code(
                    context("alice"), CodeExecutionInput(code="print(1)")
                )
            executor.close()

    def test_failed_cleanup_retries_without_reusing_a_deleted_session(self):
        first, second = FakeMachine(delete_error=True), FakeMachine()
        with patch("smol.google_adk.Machine.create", side_effect=[first, second]):
            executor = SmolCodeExecutor()
            executor.execute_code(context("alice"), CodeExecutionInput(code="print(1)"))
            executor.execute_code(context("bob"), CodeExecutionInput(code="print(1)"))
            with self.assertRaisesRegex(RuntimeError, "delete failed"):
                executor.close()
            self.assertTrue(second.deleted)
            first.delete_error = False
            executor.close()
            self.assertTrue(first.deleted)


if __name__ == "__main__":
    unittest.main()
