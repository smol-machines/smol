"""Workspace conformance rules against a real, opt-in local Smol VM."""

from __future__ import annotations

import os
from collections.abc import Awaitable, Callable, Iterator

import pytest
from pydantic_ai.workspaces import WorkspaceBackend, WorkspaceRef
from pydantic_ai.workspaces.conformance import WorkspaceBackendSuite
from smol import ConnectOptions, Machine, SmolError
from smol.pydantic_ai_harness import SmolSandbox, SmolSandboxBackend

pytestmark = pytest.mark.anyio


@pytest.mark.skipif(
    os.getenv("SMOL_SANDBOX_LIVE") != "1",
    reason="set SMOL_SANDBOX_LIVE=1 with local virtualization to run the VM conformance suite",
)
class TestLiveSmolSandboxBackend(WorkspaceBackendSuite):
    @pytest.fixture(scope="class")
    @classmethod
    def backend(cls) -> Iterator[SmolSandboxBackend]:
        backend = SmolSandboxBackend()
        try:
            yield backend
        finally:
            if backend.ref is not None:
                Machine.connect(backend.ref.id, ConnectOptions(target="local")).delete()

    @pytest.fixture
    def fresh_backend(self) -> Iterator[Callable[[], WorkspaceBackend]]:
        created: list[SmolSandboxBackend] = []

        def factory() -> SmolSandboxBackend:
            backend = SmolSandboxBackend()
            created.append(backend)
            return backend

        yield factory
        for backend in created:
            if backend.ref is not None:
                try:
                    Machine.connect(
                        backend.ref.id, ConnectOptions(target="local")
                    ).delete()
                except SmolError as error:
                    if error.code != "NOT_FOUND":
                        raise

    @pytest.fixture
    def attach_backend(self) -> Callable[[WorkspaceRef], WorkspaceBackend]:
        return lambda ref: SmolSandboxBackend(ref=ref)

    @pytest.fixture
    def destroy_environment(self) -> Callable[[WorkspaceBackend], Awaitable[None]]:
        async def destroy(backend: WorkspaceBackend) -> None:
            assert backend.ref is not None
            await SmolSandbox[None]().destroy(backend.ref)

        return destroy
