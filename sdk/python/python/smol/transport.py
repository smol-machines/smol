"""Transport layer — one ``Machine`` API over local (embedded) or cloud (smolfleet).

Mirrors the Node SDK's ``transport.ts``:

* ``LocalTransport`` wraps the native extension (``smol._native``) that links the
  smolvm engine in-process — no server.
* ``CloudTransport`` is a pure-Python REST client to smolfleet ``/v1`` (Bearer
  ``smk_…``); request/response shapes match smolfleet's OpenAPI contract.

The HTTP layer uses only the stdlib (``urllib``) so the cloud path has zero
third-party dependencies.
"""

from __future__ import annotations

import atexit
import json
import os
import time
import urllib.error
import urllib.request
import weakref
from typing import Any, Optional, Protocol
from urllib.parse import quote

from .errors import InvalidConfigError, NotSupportedError, SmolError, wrap_native_error
from .types import ConnectOptions, ExecOptions, ExecResult, ImageInfo, MachineConfig, PortSpec

DEFAULT_CLOUD_URL = "https://api.smolmachines.com"
CLOUD_TIMEOUT_S = 30.0
# Extra slack added on top of a command's own timeout when sizing the exec HTTP
# read timeout, covering network round-trip and server-side overhead so the
# client never aborts before the server has had a chance to finish the command.
CLOUD_EXEC_TIMEOUT_HEADROOM_S = 30.0


class Transport(Protocol):
    @property
    def name(self) -> str: ...
    def state(self) -> str: ...
    def url(self) -> Optional[str]: ...
    def exec(self, command: list[str], opts: Optional[ExecOptions] = None) -> ExecResult: ...
    def run(self, image: str, command: list[str], opts: Optional[ExecOptions] = None) -> ExecResult: ...
    def exec_stream(self, command: list[str], opts: Optional[ExecOptions] = None): ...
    def read_file(self, path: str) -> bytes: ...
    def write_file(self, path: str, data: bytes, mode: Optional[int] = None) -> None: ...
    def pull_image(self, image: str) -> ImageInfo: ...
    def list_images(self) -> list[ImageInfo]: ...
    def stop(self) -> None: ...
    def delete(self) -> None: ...
    def fork(self, name: str, ports: Optional[list[PortSpec]] = None) -> "Transport": ...


def _encode_path(p: str) -> str:
    """Percent-encode each segment but keep ``/`` (smolfleet files route is a
    wildcard ``/files/<path>``); escapes spaces / ? / # / % in filenames."""
    return "/".join(quote(seg, safe="") for seg in p.split("/"))


# ---------------------------------------------------------------------------
# Local (embedded engine via the native extension)
# ---------------------------------------------------------------------------
def _load_native() -> Any:
    try:
        from . import _native  # type: ignore[attr-defined]

        return _native
    except ImportError as e:  # native ext not built/installed for this platform
        raise NotSupportedError(
            "the local engine native extension is not available — build it with "
            "`maturin develop` (or install a prebuilt wheel), or use the cloud "
            "target via Machine.create(..., ConnectOptions(target='cloud'))."
        ) from e


def _native_exec_options(opts: Optional[ExecOptions]) -> Optional[dict]:
    if opts is None:
        return None
    out: dict[str, Any] = {}
    if opts.workdir is not None:
        out["workdir"] = opts.workdir
    if opts.timeout is not None:
        out["timeout_secs"] = opts.timeout
    if opts.env is not None:
        out["env"] = [{"key": k, "value": v} for k, v in opts.env.items()]
    return out


def _native_config(name: str, config: MachineConfig) -> dict:
    cfg: dict[str, Any] = {"name": name, "persistent": config.persistent}
    if config.mounts:
        cfg["mounts"] = [
            {"source": m.source, "target": m.target, "read_only": m.effective_read_only}
            for m in config.mounts
        ]
    if config.ports:
        cfg["ports"] = [{"host": p.host, "guest": p.guest} for p in config.ports]
    r = config.resources
    if r is not None:
        res: dict[str, Any] = {}
        if r.cpus is not None:
            res["cpus"] = r.cpus
        if r.memory_mb is not None:
            res["memory_mib"] = r.memory_mb
        if r.network is not None:
            res["network"] = r.network
        if r.storage_gb is not None:
            res["storage_gib"] = r.storage_gb
        if r.overlay_gb is not None:
            res["overlay_gib"] = r.overlay_gb
        if r.gpu is not None:
            res["gpu"] = r.gpu
        if r.gpu_vram_mib is not None:
            res["gpu_vram_mib"] = r.gpu_vram_mib
        if r.cuda is not None:
            res["cuda"] = r.cuda
        cfg["resources"] = res
    return cfg


def _image_info(d: Any) -> ImageInfo:
    # The native extension returns pyclass ImageInfo objects (attribute access);
    # the cloud transport never calls this (its image ops raise NotSupported).
    return ImageInfo(
        reference=d.reference,
        digest=d.digest,
        size=d.size,
        architecture=d.architecture,
        os=d.os,
    )


# Live local machines, stopped on interpreter exit so a script that forgets
# delete()/stop() — or exits via Ctrl-C (SIGINT raises KeyboardInterrupt, which
# on shutdown runs atexit) or an uncaught exception — doesn't leave the VM
# running until the engine's parent-death watchdog reaps it. This is best-effort
# GRACEFUL teardown (faster + cleaner); the watchdog remains the net for
# SIGKILL/crash. WeakSet so GC'd machines drop out on their own. Local only —
# cloud machines are remote and intentionally outlive this process.
_live_local: "weakref.WeakSet[LocalTransport]" = weakref.WeakSet()
_atexit_registered = False


def _stop_all_local() -> None:
    for t in list(_live_local):
        try:
            t._inner.stop()
        except Exception:  # noqa: BLE001 - best-effort teardown; never raise on exit
            pass


def _register_local(t: "LocalTransport") -> None:
    global _atexit_registered
    _live_local.add(t)
    if not _atexit_registered:
        atexit.register(_stop_all_local)
        _atexit_registered = True


class LocalTransport:
    def __init__(self, inner: Any) -> None:
        self._inner = inner
        _register_local(self)

    @property
    def name(self) -> str:
        return self._inner.name

    def state(self) -> str:
        return str(self._inner.state())

    def url(self) -> Optional[str]:
        # Local machines have no public ingress URL — that's a cloud feature.
        return None

    def exec(self, command: list[str], opts: Optional[ExecOptions] = None) -> ExecResult:
        try:
            r = self._inner.exec(command, _native_exec_options(opts))
        except Exception as e:  # noqa: BLE001 - re-typed below
            raise wrap_native_error(e) from e
        return ExecResult(exit_code=r.exit_code, stdout=r.stdout, stderr=r.stderr)

    def run(self, image: str, command: list[str], opts: Optional[ExecOptions] = None) -> ExecResult:
        try:
            r = self._inner.run(image, command, _native_exec_options(opts))
        except Exception as e:  # noqa: BLE001
            raise wrap_native_error(e) from e
        return ExecResult(exit_code=r.exit_code, stdout=r.stdout, stderr=r.stderr)

    def exec_stream(self, command: list[str], opts: Optional[ExecOptions] = None):
        try:
            stream = self._inner.exec_stream(command, _native_exec_options(opts))
        except Exception as e:  # noqa: BLE001
            raise wrap_native_error(e) from e
        # native ExecStream is a Python iterator of event dicts (live)
        for event in stream:
            yield event

    def read_file(self, path: str) -> bytes:
        try:
            return bytes(self._inner.read_file(path))
        except Exception as e:  # noqa: BLE001
            raise wrap_native_error(e) from e

    def write_file(self, path: str, data: bytes, mode: Optional[int] = None) -> None:
        try:
            self._inner.write_file(path, data, mode)
        except Exception as e:  # noqa: BLE001
            raise wrap_native_error(e) from e

    def pull_image(self, image: str) -> ImageInfo:
        try:
            return _image_info(self._inner.pull_image(image))
        except Exception as e:  # noqa: BLE001
            raise wrap_native_error(e) from e

    def list_images(self) -> list[ImageInfo]:
        try:
            return [_image_info(i) for i in self._inner.list_images()]
        except Exception as e:  # noqa: BLE001
            raise wrap_native_error(e) from e

    def stop(self) -> None:
        _live_local.discard(self)
        try:
            self._inner.stop()
        except Exception as e:  # noqa: BLE001
            raise wrap_native_error(e) from e

    def delete(self) -> None:
        _live_local.discard(self)
        try:
            self._inner.delete()
        except Exception as e:  # noqa: BLE001
            raise wrap_native_error(e) from e

    def fork(self, name: str, ports: Optional[list[PortSpec]] = None) -> "Transport":
        # Local live-RAM CoW clone via the embedded engine. The golden must have
        # been started forkable (MachineConfig(forkable=True)).
        pinned = [(p.host, p.guest) for p in (ports or [])]
        try:
            clone_inner = self._inner.fork(name, pinned)
        except Exception as e:  # noqa: BLE001
            raise wrap_native_error(e) from e
        # LocalTransport.__init__ registers the clone for atexit cleanup.
        return LocalTransport(clone_inner)


# ---------------------------------------------------------------------------
# Cloud (smolfleet /v1)
# ---------------------------------------------------------------------------
class CloudTransport:
    def __init__(self, base_url: str, api_key: str, machine_id: str, name: str) -> None:
        self._base = base_url
        self._key = api_key
        self._id = machine_id
        self._name = name

    @property
    def name(self) -> str:
        return self._name

    def state(self) -> str:
        m = _cloud_fetch(self._base, self._key, "GET", f"/v1/machines/{self._id}")
        return str((m or {}).get("state", "unknown"))

    def url(self) -> Optional[str]:
        # Public ingress URL for the first published port; None until the machine
        # is started with an allocated host port (and the control plane advertises
        # a public base URL).
        m = _cloud_fetch(self._base, self._key, "GET", f"/v1/machines/{self._id}")
        return (m or {}).get("url")

    def exec(self, command: list[str], opts: Optional[ExecOptions] = None) -> ExecResult:
        body = {
            "command": command,  # CommandSpec: argv array
            "env": (opts.env if opts else None) or {},
            "cwd": opts.workdir if opts else None,
            "timeoutSeconds": opts.timeout if opts else None,
        }
        # The command may legitimately run far longer than the default cloud
        # timeout, so size the HTTP read timeout off the request's own timeout
        # (plus headroom) — never below the default. The server-sent
        # timeoutSeconds above is left untouched.
        http_timeout = CLOUD_TIMEOUT_S
        if opts and opts.timeout is not None:
            http_timeout = max(CLOUD_TIMEOUT_S, opts.timeout + CLOUD_EXEC_TIMEOUT_HEADROOM_S)
        r = _cloud_fetch(
            self._base, self._key, "POST", f"/v1/machines/{self._id}/exec", json_body=body, timeout=http_timeout
        )
        r = r or {}
        return ExecResult(
            exit_code=int(r.get("exitCode", 0)),
            stdout=str(r.get("stdout", "")),
            stderr=str(r.get("stderr", "")),
        )

    def run(self, image: str, command: list[str], opts: Optional[ExecOptions] = None) -> ExecResult:
        raise NotSupportedError(
            "run(image, …) is not available on the cloud target; create a machine "
            "from an image via Machine.create(MachineConfig(image=...), "
            "ConnectOptions(target='cloud')) and use exec()."
        )

    def exec_stream(self, command: list[str], opts: Optional[ExecOptions] = None):
        body = {
            "command": command,
            "env": (opts.env if opts else None) or {},
            "cwd": opts.workdir if opts else None,
            "timeoutSeconds": opts.timeout if opts else None,
        }
        headers = {
            "authorization": f"Bearer {self._key}",
            "content-type": "application/json",
            "accept": "text/event-stream",
        }
        req = urllib.request.Request(
            f"{self._base}/v1/machines/{self._id}/exec/stream",
            data=json.dumps(body).encode(),
            headers=headers,
            method="POST",
        )
        try:
            resp = urllib.request.urlopen(req, timeout=CLOUD_TIMEOUT_S)
        except urllib.error.HTTPError as e:
            text = ""
            try:
                text = e.read().decode(errors="replace")
            except Exception:  # noqa: BLE001
                pass
            code = "NOT_FOUND" if e.code == 404 else "UNAUTHORIZED" if e.code == 401 else "SMOLVM_ERROR"
            raise SmolError(code, f"cloud POST exec/stream → {e.code}{(': ' + text) if text else ''}") from e
        except urllib.error.URLError as e:
            raise SmolError("CONNECTION", f"cloud exec/stream failed: {getattr(e, 'reason', e)}") from e

        # Parse the server's SSE stream: each event is `event: <kind>` + one or
        # more `data:` lines, terminated by a blank line. Multiple data lines join
        # with `\n` (SSE spec); the `exit` event's data is JSON `{ "exitCode": N }`.
        event = ""
        data_lines: list[str] = []

        def _flush():
            nonlocal event, data_lines
            kind = event
            payload = "\n".join(data_lines)
            event = ""
            data_lines = []
            if kind == "stdout":
                return {"kind": "stdout", "data": payload}
            if kind == "stderr":
                return {"kind": "stderr", "data": payload}
            if kind == "error":
                return {"kind": "error", "message": payload}
            if kind == "exit":
                try:
                    ec = int(json.loads(payload).get("exitCode", 0))
                except Exception:  # noqa: BLE001
                    ec = 0
                return {"kind": "exit", "exit_code": ec}
            return None

        try:
            for raw in resp:
                line = raw.decode("utf-8", "replace").rstrip("\n")
                if line.endswith("\r"):
                    line = line[:-1]
                if line == "":
                    ev = _flush()
                    if ev is not None:
                        yield ev
                elif line.startswith("event:"):
                    event = line[6:].strip()
                elif line.startswith("data:"):
                    s = line[5:]
                    if s.startswith(" "):
                        s = s[1:]
                    data_lines.append(s)
            ev = _flush()
            if ev is not None:
                yield ev
        finally:
            resp.close()

    def read_file(self, path: str) -> bytes:
        return _cloud_fetch(
            self._base, self._key, "GET", f"/v1/machines/{self._id}/files/{_encode_path(path)}", accept="bytes"
        )

    def write_file(self, path: str, data: bytes, mode: Optional[int] = None) -> None:
        _cloud_fetch(
            self._base, self._key, "PUT", f"/v1/machines/{self._id}/files/{_encode_path(path)}", raw_body=data
        )
        # The cloud /files PUT carries no file mode, so apply it with chmod when
        # requested — e.g. writing an executable script the caller then runs.
        if mode is not None:
            self.exec(["chmod", format(mode, "o"), path])

    def pull_image(self, image: str) -> ImageInfo:
        raise NotSupportedError("pull_image is not available on the cloud target.")

    def list_images(self) -> list[ImageInfo]:
        raise NotSupportedError("list_images is not available on the cloud target.")

    def stop(self) -> None:
        _cloud_fetch(self._base, self._key, "POST", f"/v1/machines/{self._id}/stop")

    def delete(self) -> None:
        _cloud_fetch(self._base, self._key, "DELETE", f"/v1/machines/{self._id}")

    def fork(self, name: str, ports: Optional[list[PortSpec]] = None) -> "CloudTransport":
        # Live-RAM CoW clone on the golden's node. The control plane returns the
        # running clone; wait for its agent so the returned handle is usable.
        port_body = [{"port": p.guest, "hostPort": p.host} for p in (ports or [])]
        clone = (
            _cloud_fetch(
                self._base,
                self._key,
                "POST",
                f"/v1/machines/{self._id}/fork",
                json_body={"name": name, "ports": port_body},
            )
            or {}
        )
        clone_id = clone["id"]
        clone_name = clone.get("name") or name
        _wait_for_ready(self._base, self._key, clone_id)
        return CloudTransport(self._base, self._key, clone_id, clone_name)


def _cloud_fetch(
    base_url: str,
    api_key: str,
    method: str,
    path: str,
    *,
    json_body: Optional[dict] = None,
    raw_body: Optional[bytes] = None,
    accept: str = "json",
    timeout: float = CLOUD_TIMEOUT_S,
) -> Any:
    headers = {"authorization": f"Bearer {api_key}"}
    data: Optional[bytes] = None
    if json_body is not None:
        headers["content-type"] = "application/json"
        data = json.dumps(json_body).encode()
    elif raw_body is not None:
        headers["content-type"] = "application/octet-stream"
        data = raw_body

    req = urllib.request.Request(f"{base_url}{path}", data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            payload = resp.read()
            if accept == "bytes":
                return payload
            ct = resp.headers.get("content-type", "")
            if resp.status == 204 or not payload:
                return None
            return json.loads(payload) if "application/json" in ct else None
    except urllib.error.HTTPError as e:
        text = ""
        try:
            text = e.read().decode(errors="replace")
        except Exception:  # noqa: BLE001
            pass
        code = "NOT_FOUND" if e.code == 404 else "UNAUTHORIZED" if e.code == 401 else "SMOLVM_ERROR"
        raise SmolError(code, f"cloud {method} {path} → {e.code}{(': ' + text) if text else ''}") from e
    except urllib.error.URLError as e:
        reason = getattr(e, "reason", e)
        raise SmolError("CONNECTION", f"cloud request failed: {reason}") from e
    except TimeoutError as e:
        raise SmolError("TIMEOUT", f"cloud {method} {path} timed out after {timeout}s") from e


def _wait_for_ready(base_url: str, api_key: str, machine_id: str, timeout_s: float = 120.0) -> None:
    deadline = time.monotonic() + timeout_s
    while True:
        state = None
        try:
            m = _cloud_fetch(base_url, api_key, "GET", f"/v1/machines/{machine_id}")
            state = (m or {}).get("state")
        except SmolError:
            pass  # transient while booting
        if state in ("started", "running"):
            return
        if state == "error":
            raise SmolError("SMOLVM_ERROR", f"machine {machine_id} entered error state while starting")
        if time.monotonic() >= deadline:
            raise SmolError("TIMEOUT", f"machine {machine_id} not ready after {timeout_s}s (state={state})")
        time.sleep(1.0)


# ---------------------------------------------------------------------------
# Factory
# ---------------------------------------------------------------------------
def _cli_config_path() -> str:
    """Path the `smol` CLI persists its config to — `~/.config/smolvm/config.toml`
    on every platform (the CLI uses `home/.config/smolvm`, not XDG or ~/Library)."""
    return os.path.join(os.path.expanduser("~"), ".config", "smolvm", "config.toml")


def _read_cli_cloud_table() -> dict[str, Any]:
    """Best-effort read of the `[cloud]` table the CLI writes on `smol auth login`,
    so an SDK process inherits that session without re-specifying credentials.
    Returns {} on any problem (missing file, no TOML parser, malformed)."""
    try:
        with open(_cli_config_path(), "rb") as f:
            raw = f.read()
    except OSError:
        return {}
    text = raw.decode("utf-8", "replace")
    for mod_name in ("tomllib", "tomli"):  # tomllib is stdlib on 3.11+
        try:
            mod = __import__(mod_name)
        except ModuleNotFoundError:
            continue
        try:
            return mod.loads(text).get("cloud", {}) or {}
        except Exception:  # noqa: BLE001 — malformed file, fall through
            return {}
    # No TOML parser available (Python <3.11 without `tomli`): scan the flat,
    # tool-written `[cloud]` table for the two string keys we need.
    return _scan_cloud_table(text)


def _scan_cloud_table(text: str) -> dict[str, Any]:
    out: dict[str, Any] = {}
    in_cloud = False
    for line in text.splitlines():
        s = line.strip()
        if s.startswith("[") and s.endswith("]"):
            in_cloud = s == "[cloud]"
            continue
        if not in_cloud or "=" not in s or s.startswith("#"):
            continue
        key, _, val = s.partition("=")
        out[key.strip()] = val.strip().strip('"').strip("'")
    return out


def _token_is_expired(expires_at: Any) -> bool:
    """True only when `token_expires_at` parses cleanly AND is in the past.
    Conservative: any parse failure returns False so a valid key is never
    blocked over a formatting quirk."""
    if not expires_at or not isinstance(expires_at, str):
        return False
    try:
        import datetime

        ts = expires_at.replace("Z", "+00:00")
        dt = datetime.datetime.fromisoformat(ts)
        if dt.tzinfo is None:
            dt = dt.replace(tzinfo=datetime.timezone.utc)
        return dt <= datetime.datetime.now(datetime.timezone.utc)
    except Exception:  # noqa: BLE001
        return False


def _cli_session() -> tuple[Optional[str], Optional[str]]:
    """`(api_key, endpoint)` from the CLI's login session, or `(None, None)`.
    An expired access token is treated as absent so the caller surfaces an
    honest "run `smol auth login`" message rather than a raw 401 later."""
    cloud = _read_cli_cloud_table()
    key = cloud.get("api_key")
    if not key or _token_is_expired(cloud.get("token_expires_at")):
        return (None, None)
    return (key, cloud.get("endpoint") or None)


# Shared, accurate guidance for the missing-credential errors below. The old
# text said "run `smol login`" — but that command doesn't exist (it's
# `smol auth login`) and it writes the token to config.toml, which the SDK now
# reads. Point users at the real, working path.
_NO_KEY_HINT = (
    "pass ConnectOptions(api_key=...), set SMOL_CLOUD_TOKEN, or run "
    "`smol auth login` to create a CLI session the SDK reuses"
)


def make_transport(config: MachineConfig, conn: Optional[ConnectOptions] = None) -> Transport:
    conn = conn or ConnectOptions()
    cli_key, cli_url = _cli_session() if conn.target != "local" else (None, None)
    api_key = conn.api_key or os.environ.get("SMOL_CLOUD_TOKEN") or cli_key
    use_cloud = conn.target == "cloud" or (conn.target != "local" and bool(api_key))

    if use_cloud:
        if not api_key:
            raise InvalidConfigError(f"cloud target requires an api_key — {_NO_KEY_HINT}.")
        if not config.image:
            raise InvalidConfigError(
                "cloud target requires an image — pass MachineConfig(image=...)."
            )
        # Host bind-mounts are a local-only concept: a remote machine has no host
        # filesystem to bind. The cloud API has no field for them, so rather than
        # silently dropping them, reject up front. (Cloud persistent storage is a
        # separate, volume-based feature, not host mounts.) Published ports, by
        # contrast, ARE a cloud feature — the control plane allocates a node host
        # port for each guest port and routes ingress to it.
        if config.mounts:
            raise NotSupportedError(
                "host mounts are local-only and are not applied on the cloud target; "
                "use cloud volumes for persistent storage instead."
            )
        base_url = (conn.base_url or os.environ.get("SMOL_CLOUD_URL") or cli_url or DEFAULT_CLOUD_URL).rstrip("/")

        r = config.resources
        resources: dict[str, Any] = {"diskGb": r.storage_gb if r else None}
        if r and r.cpus is not None:
            resources["cpus"] = r.cpus
        if r and r.memory_mb is not None:
            resources["memoryMb"] = r.memory_mb
        body: dict[str, Any] = {
            "name": config.name,
            "source": {"type": "image", "reference": config.image},
            "resources": resources,
            "autoStopSeconds": config.auto_stop_seconds,
            "ttlSeconds": config.ttl_seconds,
        }
        if r and (r.allow_cidrs or r.allow_hosts):
            body["network"] = {
                "mode": "allowCidrs",
                "cidrs": r.allow_cidrs or [],
                "hosts": r.allow_hosts or [],
            }
        elif r and r.network:
            body["network"] = {"mode": "open"}
        # Publish ports: supply only the guest port; the control plane allocates
        # the node host port (read the allocated hostPort back from the machine
        # info after start). Publishing a port implies the virtio-net backend.
        if config.ports:
            body["ports"] = [{"port": p.guest} for p in config.ports]

        created = _cloud_fetch(base_url, api_key, "POST", "/v1/machines", json_body=body) or {}
        machine_id = created["id"]
        name = created.get("name") or config.name or machine_id
        # The machine now exists on the cloud. If start/readiness fails, delete it
        # before propagating — otherwise it leaks (and bills) as an orphan.
        # A forkable golden boots with its guest RAM in a cloneable memfd so it
        # can later be forked with Machine.fork (live-RAM CoW, RL rollouts).
        start_path = f"/v1/machines/{machine_id}/start"
        if config.forkable:
            start_path += "?forkable=true"
        try:
            try:
                _cloud_fetch(base_url, api_key, "POST", start_path)
            except SmolError:
                pass  # best-effort; _wait_for_ready is the gate
            _wait_for_ready(base_url, api_key, machine_id)
        except BaseException:
            try:
                _cloud_fetch(base_url, api_key, "DELETE", f"/v1/machines/{machine_id}")
            except Exception:  # noqa: BLE001 - cleanup is best-effort; surface the original error
                pass
            raise
        return CloudTransport(base_url, api_key, machine_id, name)

    # Local embedded engine via the native extension.
    native = _load_native()
    name = config.name or _generate_name()
    try:
        inner = native.Machine(_native_config(name, config))
        # A forkable golden boots with memfd-backed guest RAM + a control socket
        # so it can be cloned with Machine.fork (local live-RAM fork).
        if config.forkable:
            inner.start_forkable()
        else:
            inner.start()
    except Exception as e:  # noqa: BLE001
        raise wrap_native_error(e) from e
    return LocalTransport(inner)


def connect_transport(machine_id: str, conn: Optional[ConnectOptions] = None) -> Transport:
    """Attach to an EXISTING machine without creating a new one — to drive a
    machine made elsewhere (another process, the console, the REST API).

    * local: re-opens a persisted machine by NAME, starting it if stopped.
    * cloud: looks up the machine by id (raises NOT_FOUND otherwise).
    """
    conn = conn or ConnectOptions()
    cli_key, cli_url = _cli_session() if conn.target != "local" else (None, None)
    api_key = conn.api_key or os.environ.get("SMOL_CLOUD_TOKEN") or cli_key
    use_cloud = conn.target == "cloud" or (conn.target != "local" and bool(api_key))
    if not use_cloud:
        # Local: start-or-reconnect to the named machine via the native engine.
        native = _load_native()
        try:
            return LocalTransport(native.Machine.connect(machine_id))
        except Exception as e:  # noqa: BLE001
            raise wrap_native_error(e) from e
    if not api_key:
        raise InvalidConfigError(f"connect requires an api_key — {_NO_KEY_HINT}.")
    base_url = (conn.base_url or os.environ.get("SMOL_CLOUD_URL") or cli_url or DEFAULT_CLOUD_URL).rstrip("/")
    # Resolve like the CLI does: try the id path first, and when that 404s,
    # list machines and match by NAME. `machine.name` returns the human name,
    # so `Machine.connect(other.name)` — the natural composition of this API —
    # must work, not just the raw `mach-…` id.
    try:
        m = _cloud_fetch(base_url, api_key, "GET", f"/v1/machines/{machine_id}") or {}
    except SmolError as e:
        if "404" not in str(e):
            raise
        listed = _cloud_fetch(base_url, api_key, "GET", "/v1/machines") or {}
        machines = listed.get("machines", listed) if isinstance(listed, dict) else listed
        hit = next(
            (x for x in (machines or []) if x.get("name") == machine_id or x.get("id") == machine_id),
            None,
        )
        if hit is None:
            raise
        m = hit
    return CloudTransport(base_url, api_key, str(m.get("id", machine_id)), str(m.get("name", machine_id)))


def _generate_name() -> str:
    import secrets

    return f"smol-{secrets.token_hex(4)}"
