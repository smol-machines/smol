"""Cloud managed agent sessions, driven through smolfleet's /v1/agents API."""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Iterator, Optional

from .errors import InvalidConfigError, NotSupportedError, SmolError
from .machine import Machine
from .transport import (
    CLOUD_START_TIMEOUT_S,
    CLOUD_TIMEOUT_S,
    DEFAULT_CLOUD_URL,
    _cli_config_api_key,
    _cli_session,
    _cloud_fetch,
)
from .types import ConnectOptions


def _connection(conn: Optional[ConnectOptions]) -> tuple[str, str]:
    conn = conn or ConnectOptions(target="cloud")
    if conn.target == "local":
        raise NotSupportedError("managed agent sessions require the cloud target")
    cli_key, cli_url = _cli_session()
    key = conn.api_key or os.environ.get("SMOL_CLOUD_TOKEN") or cli_key or _cli_config_api_key()
    if not key:
        raise InvalidConfigError("cloud target requires an API key; pass ConnectOptions(api_key=...), set SMOL_CLOUD_TOKEN, or run `smol auth login`")
    url = (conn.base_url or os.environ.get("SMOL_CLOUD_URL") or cli_url or DEFAULT_CLOUD_URL).rstrip("/")
    return url, key


def _path(name: str) -> str:
    if not name or "/" in name:
        raise InvalidConfigError("invalid agent session name")
    return "/v1/agents/" + urllib.parse.quote(name, safe="")


class AgentSession:
    """A cloud agent session. Operations return the API's camelCase dictionaries."""

    def __init__(self, name: str, base_url: str, api_key: str) -> None:
        self.name = name
        self._url = base_url
        self._key = api_key

    @classmethod
    def create(
        cls,
        name: str,
        *,
        harness: str = "claude-code",
        credential: Optional[str] = None,
        model: Optional[str] = None,
        image: Optional[str] = None,
        program: Optional[list[str]] = None,
        allow_hosts: Optional[list[str]] = None,
        open_network: bool = False,
        checkpoints: bool = True,
        cpus: Optional[int] = None,
        memory_mb: Optional[int] = None,
        arch: Optional[str] = None,
        conn: Optional[ConnectOptions] = None,
    ) -> "AgentSession":
        url, key = _connection(conn)
        body: dict[str, Any] = {"name": name, "harness": harness, "checkpoints": checkpoints}
        for wire, value in (
            ("credential", credential), ("model", model), ("image", image),
            ("program", program), ("allowHosts", allow_hosts), ("cpus", cpus),
            ("memoryMb", memory_mb), ("arch", arch),
        ):
            if value is not None:
                body[wire] = value
        if open_network:
            body["openNetwork"] = True
        info = _cloud_fetch(url, key, "POST", "/v1/agents", json_body=body)
        return cls(info["name"], url, key)

    @classmethod
    def connect(cls, name: str, conn: Optional[ConnectOptions] = None) -> "AgentSession":
        url, key = _connection(conn)
        _cloud_fetch(url, key, "GET", _path(name))
        return cls(name, url, key)

    @staticmethod
    def list(conn: Optional[ConnectOptions] = None, *, after: Optional[str] = None, limit: Optional[int] = None) -> dict[str, Any]:
        url, key = _connection(conn)
        query = urllib.parse.urlencode({k: v for k, v in (("after", after), ("limit", limit)) if v is not None})
        return _cloud_fetch(url, key, "GET", "/v1/agents" + ("?" + query if query else ""))

    def info(self) -> dict[str, Any]:
        return _cloud_fetch(self._url, self._key, "GET", _path(self.name))

    def machine(self) -> Machine:
        """Attach to the machine to stage files or inspect the workspace."""
        machine_id = self.info().get("machineId")
        if not machine_id:
            raise SmolError("INVALID_STATE", "agent session has no machine yet")
        return Machine.connect(machine_id, ConnectOptions(target="cloud", base_url=self._url, api_key=self._key))

    def send(
        self,
        prompt: str,
        *,
        env: Optional[dict[str, str]] = None,
        timeout_seconds: Optional[int] = None,
        idempotency_key: Optional[str] = None,
    ) -> int:
        body: dict[str, Any] = {"prompt": prompt}
        if env is not None:
            body["env"] = env
        if timeout_seconds is not None:
            body["timeoutSeconds"] = timeout_seconds
        headers = {"Idempotency-Key": idempotency_key} if idempotency_key is not None else None
        result = _cloud_fetch(self._url, self._key, "POST", _path(self.name) + "/turns", json_body=body, extra_headers=headers)
        return int(result["turn"])

    def events(self, turn: int, *, after: Optional[int] = None) -> Iterator[dict[str, Any]]:
        """Replay and follow a turn; reconnect with ``after`` after a network break."""
        path = _path(self.name) + f"/turns/{turn}/events"
        if after is not None:
            path += "?after=" + str(after)
        req = urllib.request.Request(self._url + path, headers={"Authorization": "Bearer " + self._key, "Accept": "text/event-stream"})
        try:
            with urllib.request.urlopen(req, timeout=max(CLOUD_TIMEOUT_S, 120.0)) as response:
                kind = ""
                event_id = 0
                data: list[str] = []
                finished = False
                for raw in response:
                    if len(raw) > 2 * 1024 * 1024:
                        raise SmolError("SMOLVM_ERROR", "agent event frame is too large")
                    line = raw.decode("utf-8", "replace").rstrip("\r\n")
                    if line.startswith("event:"):
                        kind = line[6:].strip()
                    elif line.startswith("id:"):
                        event_id = int(line[3:].strip())
                    elif line.startswith("data:"):
                        data.append(line[5:].lstrip(" "))
                    elif line == "":
                        payload = "\n".join(data)
                        if kind == "event":
                            yield {"type": "event", "id": event_id, "data": json.loads(payload)}
                        elif kind == "done":
                            finished = True
                            yield {"type": "done", "turn": json.loads(payload)}
                            return
                        elif kind == "error":
                            raise SmolError("SMOLVM_ERROR", payload)
                        kind, event_id, data = "", 0, []
                if not finished:
                    raise SmolError("CONNECTION", "agent event stream ended before the turn finished; reconnect with after")
        except urllib.error.HTTPError as error:
            raise SmolError("NOT_FOUND" if error.code == 404 else "SMOLVM_ERROR", f"cloud GET {path} → {error.code}: {error.read().decode(errors='replace')}") from error
        except urllib.error.URLError as error:
            raise SmolError("CONNECTION", f"agent event stream failed: {error.reason}") from error

    def cancel(self, turn: int) -> None:
        _cloud_fetch(self._url, self._key, "POST", _path(self.name) + f"/turns/{turn}/cancel", timeout=CLOUD_START_TIMEOUT_S)

    def rewind(self, turn: int) -> dict[str, Any]:
        return _cloud_fetch(self._url, self._key, "POST", _path(self.name) + "/rewind", json_body={"turn": turn}, timeout=CLOUD_START_TIMEOUT_S)

    def branch(self, turn: int, name: str) -> "AgentSession":
        body = {"turn": turn, "name": name}
        try:
            info = _cloud_fetch(self._url, self._key, "POST", _path(self.name) + "/branch", json_body=body, timeout=CLOUD_START_TIMEOUT_S)
        except SmolError as error:
            if error.code != "NOT_FOUND":
                raise
            info = _cloud_fetch(self._url, self._key, "POST", _path(self.name) + "/fork", json_body=body, timeout=CLOUD_START_TIMEOUT_S)
        return AgentSession(info["name"], self._url, self._key)

    def fork(self, turn: int, name: str) -> "AgentSession":
        """Compatibility alias for branch."""
        return self.branch(turn, name)

    def pause(self) -> None:
        _cloud_fetch(self._url, self._key, "POST", _path(self.name) + "/pause", timeout=CLOUD_START_TIMEOUT_S)

    def resume(self) -> None:
        _cloud_fetch(self._url, self._key, "POST", _path(self.name) + "/resume", timeout=CLOUD_START_TIMEOUT_S)

    def delete(self) -> None:
        _cloud_fetch(self._url, self._key, "DELETE", _path(self.name), timeout=CLOUD_START_TIMEOUT_S)
