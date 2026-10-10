"""CodeAct provider contract and VM ownership without a model or hypervisor."""

from __future__ import annotations

import asyncio
import shlex
from collections.abc import Awaitable, Mapping, Sequence
from typing import Any, ClassVar

import pytest

pytest.importorskip("agent_framework_tools")

from smol.code_act import SmolCodeActProvider, SmolExecuteCodeTool
from smol.types import ConnectOptions, ExecResult


class FakeMachine:
    created: ClassVar[list[tuple[object, object]]] = []
    instances: ClassVar[list[FakeMachine]] = []

    def __init__(self, *, stdout: str = "hello\n", exit_code: int = 0) -> None:
        self.commands: list[tuple[list[str], object]] = []
        self.stdout = stdout
        self.exit_code = exit_code
        self.deleted = False
        self.instances.append(self)

    @classmethod
    async def create(cls, config: object, conn: object) -> FakeMachine:
        cls.created.append((config, conn))
        return cls()

    async def exec(self, command: list[str], opts: object) -> ExecResult:
        self.commands.append((command, opts))
        return ExecResult(exit_code=self.exit_code, stdout=self.stdout, stderr="")

    async def delete(self) -> None:
        self.deleted = True


@pytest.fixture(autouse=True)
def fake_machine(monkeypatch: pytest.MonkeyPatch) -> None:
    FakeMachine.instances.clear()
    FakeMachine.created.clear()
    monkeypatch.setattr("smol.agent_framework.AsyncMachine", FakeMachine)


def test_code_is_one_quoted_argument_and_vm_is_disposable() -> None:
    async def scenario() -> None:
        code = "print('hello'); import os\nprint(os.getcwd())"
        async with SmolExecuteCodeTool() as execute:
            result = await execute.run(code)
            machine = FakeMachine.instances[0]
            argv, options = machine.commands[0]
            assert argv[:2] == ["/bin/sh", "-c"]
            assert shlex.split(argv[2]) == ["python3", "-c", code]
            assert options.workdir == "/workspace"
            assert result.stdout == "hello\n"
            assert not machine.deleted
            assert FakeMachine.created[0][0].image == "python:3.12-alpine"
            assert FakeMachine.created[0][0].resources.network is False
        assert machine.deleted
    asyncio.run(scenario())


def test_cloud_ttl_and_caller_owned_machine() -> None:
    async def scenario() -> None:
        conn = ConnectOptions(target="cloud")
        async with SmolExecuteCodeTool(conn=conn) as execute:
            await execute.run("print(1)")
            assert FakeMachine.created[0][0].ttl_seconds == 3600
            assert FakeMachine.created[0][1] is conn
        provided = FakeMachine()
        async with SmolExecuteCodeTool(machine=provided) as execute:
            await execute.run("print(2)")
        assert not provided.deleted
        assert len(FakeMachine.created) == 1
    asyncio.run(scenario())


def test_provider_injects_one_approved_code_tool_per_run() -> None:
    async def scenario() -> None:
        class Context:
            def __init__(self) -> None:
                self.tools: list[object] = []
                self.instructions = ""

            def extend_tools(self, source_id: str, tools: list[object]) -> None:
                assert source_id == "smol_codeact"
                self.tools.extend(tools)

            def extend_instructions(self, source_id: str, instructions: str) -> None:
                assert source_id == "smol_codeact"
                self.instructions += instructions

        async with SmolCodeActProvider() as provider:
            context = Context()
            state: dict[str, object] = {}
            await provider.before_run(agent=None, session=None, context=context, state=state)
            assert not state
            assert len(context.tools) == 1
            execute = context.tools[0]
            assert execute.name == "execute_code"
            assert execute.approval_mode == "never_require"
            assert "persist between calls" in context.instructions
            result = await execute.invoke(arguments={"code": "print('hello')"})
            assert result[0].additional_properties["exit_code"] == 0
            assert "hello" in result[0].text
        assert FakeMachine.instances[0].deleted
    asyncio.run(scenario())


def test_empty_code_is_rejected_before_guest_execution() -> None:
    async def scenario() -> None:
        async with SmolExecuteCodeTool() as execute:
            with pytest.raises(ValueError, match="non-empty"):
                await execute.run(" \n")
            assert not FakeMachine.instances[0].commands
    asyncio.run(scenario())


def test_agent_run_invokes_codeact_tool_without_external_model() -> None:
    from agent_framework import (
        Agent,
        BaseChatClient,
        ChatResponse,
        ChatResponseUpdate,
        Content,
        FunctionInvocationLayer,
        Message,
        ResponseStream,
    )

    class CodeClient(FunctionInvocationLayer[Any], BaseChatClient[Any]):
        def __init__(self) -> None:
            FunctionInvocationLayer.__init__(self)
            BaseChatClient.__init__(self)
            self.calls = 0

        def _inner_get_response(
            self,
            *,
            messages: Sequence[Message],
            stream: bool,
            options: Mapping[str, Any],
            **kwargs: Any,
        ) -> Awaitable[ChatResponse] | ResponseStream[ChatResponseUpdate, ChatResponse]:
            assert not stream

            async def response() -> ChatResponse:
                self.calls += 1
                if self.calls == 1:
                    return ChatResponse(messages=Message(
                        role="assistant",
                        contents=[Content.from_function_call(
                            call_id="execute_code_call",
                            name="execute_code",
                            arguments={"code": "print('hello')"},
                        )],
                    ))
                results = [
                    content for message in messages
                    for content in message.contents if content.type == "function_result"
                ]
                assert len(results) == 1
                assert "hello" in str(results[0].result)
                return ChatResponse(messages=Message(role="assistant", contents=["complete"]))

            return response()

    async def scenario() -> None:
        client = CodeClient()
        async with SmolCodeActProvider() as provider:
            agent = Agent(client=client, context_providers=[provider])
            answer = await agent.run("Run Python code")
            assert answer.text == "complete"
            assert client.calls == 2
        assert FakeMachine.instances[0].deleted
    asyncio.run(scenario())
