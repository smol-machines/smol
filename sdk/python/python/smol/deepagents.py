"""Deep Agents sandbox backed by a local or cloud Smol Machine.

Install ``smolmachines[deepagents]`` before importing this module. Supply a
machine with a Python 3 image: ``BaseSandbox`` uses ``python3`` inside the guest
for file listing, searching, and editing.
"""

from __future__ import annotations

from pathlib import PurePosixPath

from deepagents.backends.protocol import (
    ExecuteResponse,
    FileDownloadResponse,
    FileUploadResponse,
)
from deepagents.backends.sandbox import BaseSandbox

from .errors import SmolError
from .machine import Machine
from .types import ExecOptions


class SmolSandbox(BaseSandbox):
    """Give Deep Agents a shell and filesystem inside an existing microVM.

    The caller owns the machine lifecycle. Pass a local ``Machine`` for an
    account-free sandbox or a cloud ``Machine`` for a managed workspace.
    """

    def __init__(self, machine: Machine) -> None:
        self.machine = machine

    @property
    def id(self) -> str:
        """Return the stable machine identifier."""
        return self.machine.id

    def execute(self, command: str, *, timeout: int | None = None) -> ExecuteResponse:
        """Run a shell command in the microVM, with a bounded default timeout."""
        if timeout is not None and timeout <= 0:
            raise ValueError("timeout must be greater than zero")
        result = self.machine.exec(
            ["/bin/sh", "-lc", command],
            ExecOptions(timeout=timeout if timeout is not None else 1800),
        )
        output = result.stdout
        if result.stderr:
            output = f"{output}\n{result.stderr}" if output else result.stderr
        return ExecuteResponse(
            output=output,
            exit_code=result.exit_code,
            truncated=result.stdout_truncated or result.stderr_truncated,
        )

    def download_files(self, paths: list[str]) -> list[FileDownloadResponse]:
        """Read absolute guest paths, retaining per-file errors."""
        responses = []
        for path in paths:
            if not path.startswith("/"):
                responses.append(
                    FileDownloadResponse(path=path, content=None, error="invalid_path")
                )
                continue
            try:
                responses.append(
                    FileDownloadResponse(
                        path=path, content=self.machine.read_file(path), error=None
                    )
                )
            except (
                FileNotFoundError,
                IsADirectoryError,
                PermissionError,
                SmolError,
            ) as error:
                responses.append(
                    FileDownloadResponse(
                        path=path, content=None, error=_file_error(error)
                    )
                )
        return responses

    def upload_files(self, files: list[tuple[str, bytes]]) -> list[FileUploadResponse]:
        """Write bytes to absolute guest paths, retaining per-file errors."""
        responses = []
        for path, content in files:
            if not path.startswith("/"):
                responses.append(FileUploadResponse(path=path, error="invalid_path"))
                continue
            try:
                parent = str(PurePosixPath(path).parent)
                directory = self.machine.exec(["mkdir", "-p", parent])
                if directory.exit_code != 0:
                    error = (
                        "permission_denied"
                        if "permission denied" in directory.stderr.lower()
                        else directory.stderr or f"mkdir exited {directory.exit_code}"
                    )
                    responses.append(FileUploadResponse(path=path, error=error))
                    continue
                self.machine.write_file(path, content)
                responses.append(FileUploadResponse(path=path, error=None))
            except (
                FileNotFoundError,
                IsADirectoryError,
                PermissionError,
                SmolError,
            ) as error:
                responses.append(
                    FileUploadResponse(path=path, error=_file_error(error))
                )
        return responses


def _file_error(error: OSError | SmolError) -> str:
    if isinstance(error, IsADirectoryError):
        return "is_directory"
    if isinstance(error, PermissionError):
        return "permission_denied"
    if isinstance(error, FileNotFoundError):
        return "file_not_found"
    if isinstance(error, SmolError):
        if error.code == "IS_DIRECTORY":
            return "is_directory"
        if error.code in {"FORBIDDEN", "PERMISSION_DENIED"}:
            return "permission_denied"
        if error.code == "NOT_FOUND":
            return "file_not_found"
        if error.code == "SMOLVM_ERROR":
            message = str(error).lower()
            if "is a directory" in message:
                return "is_directory"
            if "permission denied" in message:
                return "permission_denied"
            if "no such file or directory" in message:
                return "file_not_found"
    raise error
