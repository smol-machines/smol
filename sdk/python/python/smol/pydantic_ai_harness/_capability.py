"""Capability that supplies a local or cloud Smol microVM as the run's workspace."""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from typing import Literal

from pydantic_ai.capabilities import AbstractCapability
from pydantic_ai.exceptions import UserError
from pydantic_ai.tools import AgentDepsT, RunContext
from pydantic_ai.workspaces import WorkspaceBackend, WorkspaceRef

from smol.pydantic_ai_harness import _backend
from smol.pydantic_ai_harness._backend import SmolSandboxBackend
from smol.pydantic_ai_harness._helpers import check_integer, check_working_dir


@dataclass(kw_only=True)
class SmolSandbox(AbstractCapability[AgentDepsT]):
    """Supply a Smol Machines VM to workspace-aware tools such as `Coder` or `Shell`."""

    target: Literal["local", "cloud"] = "local"
    image: str = "alpine:3.20"
    memory_mb: int = 1024
    network: bool = False
    working_dir: str = "/workspace"
    env: Mapping[str, str] | None = field(default=None, repr=False)
    api_key: str | None = field(default=None, repr=False)
    base_url: str | None = None
    ttl_seconds: int = 3600
    ready_timeout: float = 120

    def __post_init__(self) -> None:
        if self.defer_loading:
            raise UserError(
                "`SmolSandbox` does not support `defer_loading=True`: the workspace is selected first."
            )
        if self.target not in ("local", "cloud"):
            raise UserError("target must be 'local' or 'cloud'")
        check_integer("memory_mb", self.memory_mb, minimum=256)
        check_integer("ttl_seconds", self.ttl_seconds)
        check_working_dir(self.working_dir)

    def backend(self, ref: WorkspaceRef) -> SmolSandboxBackend:
        """Attach lazily to a VM, retaining its identity for the caller to manage."""
        return self._build(ref)

    async def destroy(self, ref: WorkspaceRef) -> None:
        """Delete a VM by reference; a failed deletion leaves its ref available to retry."""
        await _backend.destroy_machine(
            ref, target=self.target, base_url=self.base_url, api_key=self.api_key
        )

    def get_workspace(
        self, ctx: RunContext[AgentDepsT], *, ref: WorkspaceRef | None
    ) -> WorkspaceBackend | None:
        """Return a lazy backend if this run uses a Smol workspace."""
        del ctx
        if ref is not None and ref.provider != "smol":
            return None
        return self._build(ref)

    def _build(self, ref: WorkspaceRef | None) -> SmolSandboxBackend:
        return SmolSandboxBackend(
            ref=ref,
            target=self.target,
            image=self.image,
            memory_mb=self.memory_mb,
            network=self.network,
            working_dir=self.working_dir,
            env=self.env,
            api_key=self.api_key,
            base_url=self.base_url,
            ttl_seconds=self.ttl_seconds,
            ready_timeout=self.ready_timeout,
        )
