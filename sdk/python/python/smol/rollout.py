"""Framework-aware fused rollout client for TRL, Unsloth, and custom RL loops."""

from __future__ import annotations

import hashlib
import json
import os
import ssl
import struct
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any, Iterable, Optional, Sequence, Union


class RolloutError(RuntimeError):
    """A structured error returned by the rollout service."""

    def __init__(self, status: int, code: str, message: str):
        super().__init__(f"{code}: {message}")
        self.status = status
        self.code = code
        self.message = message


def adapter_sha256(directory: Union[str, os.PathLike]) -> str:
    """Return the deterministic directory digest required for publication."""

    root = Path(directory).resolve(strict=True)
    if not root.is_dir():
        raise ValueError("adapter path must be a directory")
    files = []
    for current, directories, names in os.walk(root, followlinks=False):
        current_path = Path(current)
        for name in directories:
            if (current_path / name).is_symlink():
                raise ValueError("adapter directories cannot contain symlinks")
        for name in names:
            path = current_path / name
            if path.is_symlink() or not path.is_file():
                raise ValueError("adapter directories may contain only regular files")
            files.append(path)
            if len(files) > 4096:
                raise ValueError("adapter contains more than 4096 files")
    if not files:
        raise ValueError("adapter directory contains no files")

    total = 0
    digest = hashlib.sha256()
    for path in sorted(files, key=lambda item: item.relative_to(root).as_posix()):
        name = path.relative_to(root).as_posix().encode("utf-8")
        size = path.stat().st_size
        total += size
        if total > 32 * 1024 * 1024 * 1024:
            raise ValueError("adapter exceeds 32 GiB")
        digest.update(struct.pack("<Q", len(name)))
        digest.update(name)
        digest.update(struct.pack("<Q", size))
        with path.open("rb") as stream:
            while True:
                chunk = stream.read(1024 * 1024)
                if not chunk:
                    break
                digest.update(chunk)
    return digest.hexdigest()


def _segment(value: str) -> str:
    return urllib.parse.quote(value, safe="")


class RolloutClient:
    """Synchronous client for the framework generation boundary."""

    def __init__(
        self,
        api_url: str,
        executor: str,
        *,
        timeout: float = 300.0,
        ssl_context: Optional[ssl.SSLContext] = None,
    ) -> None:
        self.api_url = api_url.rstrip("/")
        self.executor = executor
        self.timeout = timeout
        self.ssl_context = ssl_context

    def _request(
        self,
        method: str,
        path: str,
        body: Optional[dict[str, Any]] = None,
    ) -> Any:
        data = None if body is None else json.dumps(body, separators=(",", ":")).encode()
        request = urllib.request.Request(
            f"{self.api_url}{path}",
            data=data,
            method=method,
            headers={"content-type": "application/json"},
        )
        try:
            with urllib.request.urlopen(
                request, timeout=self.timeout, context=self.ssl_context
            ) as response:
                payload = response.read()
                return None if not payload else json.loads(payload)
        except urllib.error.HTTPError as error:
            raw = error.read()
            try:
                payload = json.loads(raw)
            except (UnicodeDecodeError, json.JSONDecodeError):
                payload = {"code": "HTTP_ERROR", "error": raw.decode(errors="replace")}
            raise RolloutError(
                error.code,
                str(payload.get("code", "HTTP_ERROR")),
                str(payload.get("error", error.reason)),
            ) from error
        except urllib.error.URLError as error:
            raise RolloutError(0, "UNAVAILABLE", str(error.reason)) from error

    @property
    def _executor_path(self) -> str:
        return f"/rollout-executors/{_segment(self.executor)}"

    def ensure_vllm_executor(
        self,
        *,
        endpoint: str,
        adapter_root: Union[str, os.PathLike],
        device_adapter_socket: Optional[Union[str, os.PathLike]] = None,
        fallback_pool: Optional[str] = None,
        max_concurrent_requests: int = 32,
        max_queue_depth: int = 256,
        request_timeout_secs: int = 300,
    ) -> dict[str, Any]:
        """Create the executor or verify an identical registration."""

        desired: dict[str, Any] = {
            "name": self.executor,
            "backend": "vllm",
            "endpoint": endpoint,
            "adapterRoot": str(Path(adapter_root).resolve()),
            "maxConcurrentRequests": max_concurrent_requests,
            "maxQueueDepth": max_queue_depth,
            "requestTimeoutSecs": request_timeout_secs,
        }
        if fallback_pool is not None:
            desired["fallbackPool"] = fallback_pool
        if device_adapter_socket is not None:
            desired["deviceAdapterSocket"] = str(Path(device_adapter_socket).resolve())
        try:
            return self._request("POST", "/rollout-executors", desired)
        except RolloutError as error:
            if error.status != 409:
                raise
        current = self.info()
        comparable = {
            "backend": current["backend"],
            "endpoint": current["endpoint"],
            "adapterRoot": current["adapterRoot"],
            "deviceAdapterSocket": current.get("deviceAdapterSocket"),
            "fallbackPool": current.get("fallbackPool"),
            "maxConcurrentRequests": current["maxConcurrentRequests"],
            "maxQueueDepth": current["maxQueueDepth"],
            "requestTimeoutSecs": current["requestTimeoutSecs"],
        }
        expected = {
            "backend": "vllm",
            "endpoint": endpoint,
            "adapterRoot": desired["adapterRoot"],
            "deviceAdapterSocket": desired.get("deviceAdapterSocket"),
            "fallbackPool": fallback_pool,
            "maxConcurrentRequests": max_concurrent_requests,
            "maxQueueDepth": max_queue_depth,
            "requestTimeoutSecs": request_timeout_secs,
        }
        if comparable != expected:
            raise RolloutError(
                409,
                "CONFLICT",
                f"executor {self.executor!r} exists with different configuration",
            )
        return current

    def info(self) -> dict[str, Any]:
        """Return capabilities, queue state, fallback, and published policies."""

        return self._request("GET", self._executor_path)

    def publish_policy(
        self,
        policy: str,
        version: str,
        adapter_directory: Union[str, os.PathLike],
        *,
        retain_previous: bool = False,
    ) -> dict[str, Any]:
        """Content-verify and atomically publish one immutable LoRA version."""

        info = self.info()
        root = Path(info["adapterRoot"]).resolve(strict=True)
        adapter = Path(adapter_directory).resolve(strict=True)
        try:
            relative = adapter.relative_to(root)
        except ValueError as error:
            raise ValueError("adapter must be beneath the executor adapter root") from error
        return self._request(
            "POST",
            f"{self._executor_path}/policies",
            {
                "policy": policy,
                "version": version,
                "adapterPath": relative.as_posix(),
                "adapterSha256": adapter_sha256(adapter),
                "retainPrevious": retain_previous,
            },
        )

    def publish_device_policy(
        self,
        policy: str,
        version: str,
        tensor_bundle_token: Union[bytes, str],
        *,
        retain_previous: bool = False,
    ) -> dict[str, Any]:
        """Atomically publish a one-use device-resident LoRA allocation."""

        token = (
            tensor_bundle_token.hex()
            if isinstance(tensor_bundle_token, bytes)
            else tensor_bundle_token
        )
        if not isinstance(token, str) or len(token) != 64:
            raise ValueError("tensor bundle token must contain exactly 32 bytes")
        try:
            token = bytes.fromhex(token).hex()
        except ValueError as error:
            raise ValueError("tensor bundle token must be hexadecimal") from error
        path = f"{self._executor_path}/device-policies"
        body = {
            "policy": policy,
            "version": version,
            "tensorBundleToken": token,
            "retainPrevious": retain_previous,
        }
        try:
            return self._request("POST", path, body)
        except RolloutError as error:
            if error.status != 0:
                raise
            # The controller remembers the consumed token's digest, making an
            # ambiguous transport retry idempotent for this exact publication.
            return self._request("POST", path, body)

    def retire_policy(self, policy: str, version: str) -> None:
        """Stop routing and unload a policy version after requests drain."""

        self._request(
            "DELETE",
            f"{self._executor_path}/policies/{_segment(policy)}/{_segment(version)}",
        )

    @staticmethod
    def _prompts(
        prompts: Union[Sequence[str], Sequence[Sequence[int]]]
    ) -> list[dict[str, Any]]:
        if not prompts:
            raise ValueError("at least one prompt is required")
        if isinstance(prompts[0], str):
            if not all(isinstance(prompt, str) for prompt in prompts):
                raise ValueError("one request cannot mix text and token prompts")
            return [{"text": prompt} for prompt in prompts]
        if not all(not isinstance(prompt, str) for prompt in prompts):
            raise ValueError("one request cannot mix text and token prompts")
        return [{"tokenIds": list(prompt)} for prompt in prompts]

    def job(
        self,
        *,
        idempotency_key: str,
        policy: str,
        prompts: Union[Sequence[str], Sequence[Sequence[int]]],
        max_tokens: int,
        version: Optional[str] = None,
        n: int = 1,
        deadline_ms: Optional[int] = None,
        **sampling: Any,
    ) -> dict[str, Any]:
        """Build a generation job for ``generate`` or a fused cohort."""

        parameters = {"n": n, "maxTokens": max_tokens}
        wire_names = {
            "temperature": "temperature",
            "top_p": "topP",
            "top_k": "topK",
            "min_p": "minP",
            "repetition_penalty": "repetitionPenalty",
            "seed": "seed",
            "logprobs": "logprobs",
            "prompt_logprobs": "promptLogprobs",
        }
        unknown = set(sampling) - set(wire_names)
        if unknown:
            raise TypeError(f"unknown sampling parameters: {sorted(unknown)}")
        parameters.update(
            {wire_names[name]: value for name, value in sampling.items() if value is not None}
        )
        job: dict[str, Any] = {
            "idempotencyKey": idempotency_key,
            "policy": policy,
            "prompts": self._prompts(prompts),
            "sampling": parameters,
        }
        if version is not None:
            job["version"] = version
        if deadline_ms is not None:
            job["deadlineMs"] = deadline_ms
        return job

    def generate(self, **job: Any) -> dict[str, Any]:
        """Generate through one policy, returning exact token IDs and logprobs."""

        return self._request("POST", f"{self._executor_path}/generate", self.job(**job))

    def generate_batch(self, jobs: Iterable[dict[str, Any]]) -> list[dict[str, Any]]:
        """Submit independent policy jobs together for backend fusion."""

        response = self._request(
            "POST", f"{self._executor_path}/batches", {"jobs": list(jobs)}
        )
        return response["jobs"]

    def close(self) -> None:
        """Drain and delete the executor registration."""

        self._request("DELETE", self._executor_path)
