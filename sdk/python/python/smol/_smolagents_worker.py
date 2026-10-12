"""Persistent Python namespace inside the guest; only listens on a guest-local socket."""

from __future__ import annotations

import ast
import contextlib
import io
import json
import os
import signal
import socket
import sys
import traceback

_MAX_REQUEST = 8 * 1024 * 1024
_MAX_OUTPUT = 256 * 1024


def _receive(conn: socket.socket) -> bytes:
    header = bytearray()
    while len(header) < 8:
        part = conn.recv(8 - len(header))
        if not part:
            raise ConnectionError("Incomplete code request header")
        header.extend(part)
    length = int.from_bytes(header, "big")
    if length > _MAX_REQUEST:
        raise ValueError("Code request exceeds the size limit")
    data = bytearray()
    while len(data) < length:
        part = conn.recv(min(65536, length - len(data)))
        if not part:
            raise ConnectionError("Incomplete code request")
        data.extend(part)
    return bytes(data)


def _timeout(_signal: int, _frame: object) -> None:
    raise TimeoutError("Python action timed out")


def serve(path: str) -> None:
    namespace: dict[str, object] = {"__name__": "__main__"}
    signal.signal(signal.SIGALRM, _timeout)
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as server:
        server.bind(path)
        os.chmod(path, 0o600)
        server.listen(1)
        while True:
            conn, _ = server.accept()
            with conn:
                out = io.StringIO()
                result: dict[str, object] = {"result": None, "final": False}
                try:
                    request = json.loads(_receive(conn))
                    if request.get("shutdown"):
                        payload = b'{"result":null,"final":false,"logs":""}'
                        conn.sendall(len(payload).to_bytes(8, "big") + payload)
                        return
                    code = request["code"]
                    duration = int(request.get("timeout", 60))
                    if duration < 1 or duration > 3600:
                        raise ValueError("Timeout must be between 1 and 3600 seconds")
                    signal.alarm(duration)
                    try:
                        with (
                            contextlib.redirect_stdout(out),
                            contextlib.redirect_stderr(out),
                        ):
                            tree = ast.parse(code)
                            if tree.body and isinstance(tree.body[-1], ast.Expr):
                                expression = ast.Expression(body=tree.body.pop().value)
                                exec(compile(tree, "<smol-agent>", "exec"), namespace)  # noqa: S102 - guest-only agent code
                                value = eval(
                                    compile(expression, "<smol-agent>", "eval"),
                                    namespace,
                                )
                                result["result"] = (
                                    repr(value) if value is not None else None
                                )
                            else:
                                exec(compile(tree, "<smol-agent>", "exec"), namespace)  # noqa: S102 - guest-only agent code
                    finally:
                        signal.alarm(0)
                except BaseException as exc:  # noqa: BLE001 - final_answer uses BaseException
                    if type(exc).__name__ == "FinalAnswerException" and hasattr(
                        exc, "value"
                    ):
                        result["final"] = True
                        result["result"] = str(exc.value)
                    else:
                        result["error"] = "".join(
                            traceback.format_exception(
                                type(exc), exc, exc.__traceback__
                            )
                        )
                result["logs"] = out.getvalue()[-_MAX_OUTPUT:]
                payload = json.dumps(result, ensure_ascii=True).encode("utf-8")
                if len(payload) > _MAX_OUTPUT * 2:
                    payload = json.dumps(
                        {
                            "error": "Python action output exceeds the size limit",
                            "logs": "",
                        }
                    ).encode()
                try:
                    conn.sendall(len(payload).to_bytes(8, "big") + payload)
                except OSError:
                    pass


if __name__ == "__main__":
    serve(sys.argv[1])
