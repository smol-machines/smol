"""Smol Machines microVM workspace for Pydantic AI agents and Harness tools."""

try:
    from ._backend import SmolSandboxBackend
    from ._capability import SmolSandbox
except ModuleNotFoundError as exc:
    if exc.name in {"pydantic_ai", "anyio"}:
        raise ImportError(
            "Install `smolmachines[pydantic-ai-harness]` to use SmolSandbox."
        ) from exc
    raise

__all__ = ("SmolSandbox", "SmolSandboxBackend")
