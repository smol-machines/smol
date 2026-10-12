"""Validation and error helpers for Smol's optional Pydantic AI integration."""

from __future__ import annotations

import asyncio
import math
import posixpath

from pydantic_ai.exceptions import UserError
from pydantic_ai.workspaces import WorkspaceCommand

try:
    import sniffio as _sniffio  # pyright: ignore[reportMissingImports]
except ModuleNotFoundError:
    _sniffio = None


def safe_credential_reason(error: Exception) -> str:
    """Classify a provider credential rejection without copying its possibly secret-bearing text."""
    message = str(error).lower()
    # Provider errors can embed the rejected token; emit only fixed labels.
    if "malformed" in message or "invalid format" in message:
        return "API key is malformed"
    if "expired" in message:
        return "Credential expired"
    if "missing" in message or "not configured" in message:
        return "Credential missing"
    return "Credentials rejected"


def running_on_asyncio() -> bool:
    """Whether the caller runs on asyncio rather than Trio.

    Inspired by AnyIO's private `current_async_library`. With `sniffio` installed, ask it: Trio records itself
    there, so the answer holds even for Trio guest mode on an asyncio loop. Without it, Trio cannot be running,
    because Trio depends on `sniffio`, so a running asyncio loop means asyncio. If Trio ever drops `sniffio`,
    only guest mode would be misread, as AnyIO would misread it too.
    """
    if _sniffio is None:
        try:
            asyncio.get_running_loop()
        except RuntimeError:
            return False
        return True
    try:
        return _sniffio.current_async_library() == "asyncio"
    except _sniffio.AsyncLibraryNotFoundError:
        return False


def absolute_path(name: str, value: str | None) -> str | None:
    """Validate an absolute POSIX path without changing symlink traversal."""
    if value is None:
        return None
    if not posixpath.isabs(value):
        raise ValueError(
            f"{name} must be an absolute workspace path or None, got {value!r}."
        )
    return value


def command_argv(command: WorkspaceCommand, shell: bool) -> list[str]:
    """The argv that runs `command`, with the same `shell` rules as core's local backend.

    A shell string runs under `/bin/sh -c`; an argv sequence runs as given. An empty argv is
    refused: a provider that quotes it with `shlex.join` would get `''`, which a shell runs as a
    successful no-op.
    """
    if isinstance(command, str):
        if not shell:
            raise TypeError(
                "a string command requires shell=True; pass an argv sequence otherwise"
            )
        return ["/bin/sh", "-c", command]
    if isinstance(command, bytes):
        raise TypeError(
            "a bytes command is not supported; pass a string or argv sequence"
        )
    if shell:
        raise TypeError(
            "an argv sequence cannot be combined with shell=True; pass a single command string"
        )
    if not command:
        raise TypeError("an argv sequence needs at least the program to run")
    for argument in command:
        if type(argument) is not str:
            raise TypeError("argv elements must be strings")
        if "\x00" in argument:
            raise ValueError("argv elements must not contain NUL bytes")
    return list(command)


def check_working_dir(value: str | None) -> None:
    """Raise `UserError` unless a provider's `working_dir` is an absolute POSIX path or `None`."""
    if value is not None and not posixpath.isabs(value):
        raise UserError(
            f"working_dir must be an absolute POSIX path or None, got {value!r}."
        )


def check_integer(
    name: str, value: int | None, *, minimum: int = 1, optional: bool = False
) -> None:
    """Raise `UserError` unless `value` is an integer of at least `minimum`, or `None` when `optional`.

    `bool` is rejected: it is an `int` subclass, but `True` is never a meant count.
    """
    if (type(value) is int and value >= minimum) or (value is None and optional):
        return
    raise UserError(
        f"{name} must be an integer of at least {minimum}{' or None' if optional else ''}, got {value!r}."
    )


def check_timeout(timeout: float | None) -> None:
    """Raise `ValueError` unless a command `timeout` is a positive finite number or `None`, as core's backends do."""
    if timeout is not None and (
        not isinstance(timeout, (int, float))
        or not math.isfinite(timeout)
        or timeout <= 0
    ):
        raise ValueError(
            f"timeout must be a positive finite number or None, got {timeout!r}."
        )
