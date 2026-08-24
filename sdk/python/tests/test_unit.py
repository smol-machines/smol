"""Pure-unit tests — no VM boot, no network. Mirrors the Node ``test/unit.ts``."""

import json
import os
import sys
import tempfile
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))

from smol.errors import ExecutionError, SmolError, wrap_native_error  # noqa: E402
from smol.rollout import RolloutClient, RolloutError, adapter_sha256  # noqa: E402
from smol import transport as transport_module  # noqa: E402
from smol.transport import _cli_config_api_key, _encode_path, _native_config  # noqa: E402
from smol.types import (  # noqa: E402
    ConnectOptions,
    ExecResult,
    MachineConfig,
    ResourceSpec,
)


def test_wrap_parses_bracketed_code():
    e = wrap_native_error(RuntimeError("[KVM_UNAVAILABLE] /dev/kvm missing"))
    assert e.code == "KVM_UNAVAILABLE"
    assert e.message == "/dev/kvm missing"


def test_wrap_unbracketed_falls_back():
    e = wrap_native_error(RuntimeError("boom"))
    assert e.code == "SMOLVM_ERROR"
    assert e.message == "boom"


def test_wrap_multiline_after_code():
    e = wrap_native_error(RuntimeError("[X] line1\nline2"))
    assert e.code == "X"
    assert e.message == "line1\nline2"


def test_wrap_passes_through_smolerror():
    orig = SmolError("CUSTOM", "already typed")
    assert wrap_native_error(orig) is orig


def test_encode_path_keeps_separators():
    assert _encode_path("/tmp/a/b.txt") == "/tmp/a/b.txt"


def test_encode_path_escapes_unsafe():
    assert _encode_path("/tmp/my file.txt") == "/tmp/my%20file.txt"
    assert _encode_path("/a/b?c#d") == "/a/b%3Fc%23d"
    assert _encode_path("/a/100%done") == "/a/100%25done"


def test_connect_bridge_root_path_has_no_trailing_slash():
    from smol.transport import CloudTransport

    c = CloudTransport("https://x", "smk_k", "mID", "n")
    base = "https://x/v1/machines/mID/connect/8080"
    # A bare root ("/", "", or no path) must NOT add a trailing slash — the
    # control routes `connect/<port>` but `connect/<port>/` matches no route.
    assert c.endpoint(8080).http_url == base
    assert c.endpoint(8080, "/").http_url == base
    assert c.endpoint(8080, "").http_url == base
    # A real sub-path is appended (leading slashes stripped, no double slash).
    assert c.endpoint(8080, "/index.html").http_url == base + "/index.html"
    assert c.endpoint(8080, "index.html").http_url == base + "/index.html"
    assert c.endpoint(8080, "//a/b").http_url == base + "/a/b"


def test_exec_result_helpers():
    ok = ExecResult(exit_code=0, stdout="hi\n", stderr="")
    assert ok.success is True
    assert ok.output == "hi\n"
    assert ok.assert_success() is ok

    bad = ExecResult(exit_code=7, stdout="", stderr="nope")
    assert bad.success is False
    assert bad.output == "nope"
    try:
        bad.assert_success(["false"])
        raise AssertionError("should have raised")
    except ExecutionError as e:
        assert e.exit_code == 7
        assert e.stderr == "nope"


def test_exec_result_truncation_defaults_false():
    r = ExecResult(exit_code=0, stdout="", stderr="")
    assert r.stdout_truncated is False
    assert r.stderr_truncated is False


def test_cli_config_api_key_fallback():
    import os
    import tempfile

    old = os.environ.get("XDG_CONFIG_HOME")
    try:
        with tempfile.TemporaryDirectory() as d:
            os.environ["XDG_CONFIG_HOME"] = d
            # No config file at all → no key.
            assert _cli_config_api_key() is None
            cfg = Path(d) / "smolvm"
            cfg.mkdir()
            (cfg / "config.toml").write_text(
                '[images]\ndefault_registry = "docker.io"\n\n'
                '[cloud]\nendpoint = "https://api.example"\napi_key = "smk_from_cli"\n'
            )
            assert _cli_config_api_key() == "smk_from_cli"
            # An api_key OUTSIDE the [cloud] section must not match.
            (cfg / "config.toml").write_text('[other]\napi_key = "smk_wrong"\n')
            assert _cli_config_api_key() is None
            # An empty key counts as absent.
            (cfg / "config.toml").write_text('[cloud]\napi_key = ""\n')
            assert _cli_config_api_key() is None
            # Malformed TOML elsewhere falls back to the line parse (also the
            # 3.9/3.10 path, where tomllib is unavailable).
            (cfg / "config.toml").write_text('[cloud]\napi_key = "smk_line"\n[broken\n')
            assert _cli_config_api_key() == "smk_line"
    finally:
        if old is None:
            os.environ.pop("XDG_CONFIG_HOME", None)
        else:
            os.environ["XDG_CONFIG_HOME"] = old


def test_native_config_forwards_gpu():
    cfg = MachineConfig(resources=ResourceSpec(gpu=True, gpu_vram_mib=512))
    res = _native_config("m", cfg)["resources"]
    assert res["gpu"] is True
    assert res["gpu_vram_mib"] == 512


def test_native_config_omits_gpu_when_unset():
    cfg = MachineConfig(resources=ResourceSpec(cpus=2))
    res = _native_config("m", cfg)["resources"]
    assert "gpu" not in res
    assert "gpu_vram_mib" not in res


def test_native_config_forwards_scoped_egress():
    cfg = MachineConfig(
        resources=ResourceSpec(
            network=True,
            allow_cidrs=["10.0.0.0/8"],
            allow_hosts=["api.example.com"],
        )
    )
    res = _native_config("m", cfg)["resources"]
    assert res["allowed_cidrs"] == ["10.0.0.0/8"]
    assert res["allowed_hosts"] == ["api.example.com"]


def test_native_config_forwards_image():
    cfg = MachineConfig(image="python:3.12-slim")
    assert _native_config("m", cfg)["image"] == "python:3.12-slim"


def test_native_config_omits_image_when_unset():
    assert "image" not in _native_config("m", MachineConfig())


def test_native_config_forwards_forkable_lifecycle():
    assert _native_config("m", MachineConfig(forkable=True))["forkable"] is True
    assert _native_config("m", MachineConfig())["forkable"] is False


def test_borrowed_local_transport_is_not_stopped_at_interpreter_exit():
    class Inner:
        name = "checkpoint"

    borrowed = transport_module.LocalTransport(Inner(), cleanup_on_exit=False)
    assert borrowed not in transport_module._live_local

    owned = transport_module.LocalTransport(Inner())
    assert owned in transport_module._live_local
    transport_module._live_local.discard(owned)


def test_connect_preserves_frozen_checkpoint_without_readiness_wait(monkeypatch):
    class Inner:
        name = "checkpoint"

        @staticmethod
        def state():
            return "frozen"

    class NativeMachine:
        @staticmethod
        def connect(name):
            assert name == "checkpoint"
            return Inner()

    class Native:
        Machine = NativeMachine

    monkeypatch.setattr(transport_module, "_load_native", lambda: Native)

    def unexpected_wait(self, *args, **kwargs):
        raise AssertionError("a frozen fork source must not wait for its agent")

    monkeypatch.setattr(
        transport_module.LocalTransport, "wait_until_ready", unexpected_wait
    )
    connected = transport_module.connect_transport(
        "checkpoint", ConnectOptions(target="local")
    )
    assert connected.state() == "frozen"


def test_native_config_forwards_image_workload_env_and_workdir():
    cfg = MachineConfig(
        image="example/service:latest",
        command=["python", "-m", "service"],
        env={"SESSION": "golden"},
        workdir="/workspace",
    )
    native = _native_config("m", cfg)
    assert native["command"] == ["python", "-m", "service"]
    assert native["env"] == {"SESSION": "golden"}
    assert native["workdir"] == "/workspace"


def test_rollout_adapter_digest_matches_engine_contract():
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        (root / "adapter_config.json").write_bytes(b"{}")
        (root / "adapter_model.safetensors").write_bytes(b"weights")
        assert adapter_sha256(root) == (
            "26d1c7593b9650cb489a9a1fe2fad9def32c75ec2685cf8261c3c0fa3b73e315"
        )


def test_rollout_job_preserves_token_prompts_and_sampling():
    client = RolloutClient("http://127.0.0.1:8080/api/v1", "qwen")
    job = client.job(
        idempotency_key="policy-a-step-4",
        policy="policy-a",
        version="step-4",
        prompts=[[1, 2, 3]],
        max_tokens=64,
        temperature=0.8,
        top_p=0.95,
    )
    assert job["prompts"] == [{"tokenIds": [1, 2, 3]}]
    assert job["sampling"] == {
        "n": 1,
        "maxTokens": 64,
        "temperature": 0.8,
        "topP": 0.95,
    }


def test_rollout_ensure_rejects_different_existing_config():
    class ExistingClient(RolloutClient):
        def _request(self, method, path, body=None):
            if method == "POST":
                raise RolloutError(409, "CONFLICT", "already exists")
            return {
                "backend": "vllm",
                "endpoint": "http://127.0.0.1:8000",
                "adapterRoot": "/adapters",
                "fallbackPool": None,
                "maxConcurrentRequests": 8,
                "maxQueueDepth": 256,
                "requestTimeoutSecs": 300,
            }

    client = ExistingClient("http://127.0.0.1:8080/api/v1", "qwen")
    try:
        client.ensure_vllm_executor(
            endpoint="http://127.0.0.1:8000",
            adapter_root="/adapters",
            max_concurrent_requests=32,
        )
        raise AssertionError("should reject a different existing configuration")
    except RolloutError as error:
        assert error.code == "CONFLICT"


def test_rollout_ensure_registers_device_adapter_socket():
    class CapturingClient(RolloutClient):
        def _request(self, method, path, body=None):
            assert method == "POST"
            assert path == "/rollout-executors"
            return body

    client = CapturingClient("http://127.0.0.1:8080/api/v1", "qwen")
    result = client.ensure_vllm_executor(
        endpoint="http://127.0.0.1:8000",
        adapter_root="/adapters",
        device_adapter_socket="/run/smolvm/device-lora.sock",
    )
    assert result["deviceAdapterSocket"] == "/run/smolvm/device-lora.sock"


def test_rollout_device_policy_normalizes_token_and_retries_ambiguity():
    class RetryingClient(RolloutClient):
        def __init__(self):
            super().__init__("http://127.0.0.1:8080/api/v1", "qwen")
            self.calls = []

        def _request(self, method, path, body=None):
            self.calls.append((method, path, body))
            if len(self.calls) == 1:
                raise RolloutError(0, "UNAVAILABLE", "response lost")
            return {"source": "device"}

    client = RetryingClient()
    result = client.publish_device_policy(
        "policy-a", "step-4", bytes(range(32)), retain_previous=True
    )
    assert result == {"source": "device"}
    assert len(client.calls) == 2
    assert client.calls[0] == client.calls[1]
    method, path, body = client.calls[0]
    assert method == "POST"
    assert path == "/rollout-executors/qwen/device-policies"
    assert body == {
        "policy": "policy-a",
        "version": "step-4",
        "tensorBundleToken": bytes(range(32)).hex(),
        "retainPrevious": True,
    }


def test_rollout_device_policy_rejects_invalid_token():
    client = RolloutClient("http://127.0.0.1:8080/api/v1", "qwen")
    for token in (b"short", "z" * 64, "00" * 31):
        try:
            client.publish_device_policy("policy", "version", token)
            raise AssertionError("should reject an invalid token")
        except ValueError:
            pass


class RecordingRolloutClient(RolloutClient):
    def __init__(self, **options):
        super().__init__("http://127.0.0.1:8080/api/v1", "qwen", **options)
        self.recorded = None

    def _request(self, method, path, body=None):
        self.recorded = (method, path, body)
        return {"ok": True}


def test_rollout_discovers_lease_and_authenticates_without_arguments():
    with tempfile.TemporaryDirectory() as directory:
        assignment = Path(directory) / "fork-env"
        assignment.write_text(
            "SMOLVM_ROLLOUT_URL=http://100.96.0.1:10081/api/v1/rollout-executors/fused\n"
            "SMOLVM_ROLLOUT_TOKEN=lease-id.secret\n"
            "SMOLVM_ROLLOUT_EXECUTOR=fused\n"
            "SMOLVM_ROLLOUT_POLICY=experiment-a\n"
            "SMOLVM_FORK_BATCH_ID=batch-a\n"
            "SMOLVM_FORK_BATCH_SIZE=2\n",
            encoding="utf-8",
        )
        with mock.patch.dict(os.environ, {}, clear=True):
            client = RolloutClient(fork_env_path=assignment)
        assert client.api_url == "http://100.96.0.1:10081/api/v1"
        assert client.executor == "fused"
        assert client.bearer_token == "lease-id.secret"
        assert client.lease_policy == "experiment-a"

        with mock.patch("urllib.request.urlopen") as urlopen:
            response = urlopen.return_value.__enter__.return_value
            response.read.return_value = b"{}"
            client.generate(
                idempotency_key="request",
                policy="experiment-a",
                prompts=["hello"],
                max_tokens=1,
            )
            request = urlopen.call_args.args[0]
            assert request.get_header("Authorization") == "Bearer lease-id.secret"
            cohort = json.loads(request.data)["cohort"]
            assert cohort["id"].startswith("fork-")
            assert cohort["size"] == 2
            assert cohort["maxWaitMs"] == 250


def test_rollout_lease_environment_overrides_assignment():
    with tempfile.TemporaryDirectory() as directory:
        assignment = Path(directory) / "fork-env"
        assignment.write_text(
            "SMOLVM_ROLLOUT_URL=http://host/api/v1/rollout-executors/file\n"
            "SMOLVM_ROLLOUT_TOKEN=file-token\n"
            "SMOLVM_ROLLOUT_EXECUTOR=file\n"
            "SMOLVM_ROLLOUT_POLICY=file-policy\n",
            encoding="utf-8",
        )
        environment = {
            "SMOLVM_ROLLOUT_URL": "http://host/api/v1/rollout-executors/env",
            "SMOLVM_ROLLOUT_TOKEN": "env-token",
            "SMOLVM_ROLLOUT_EXECUTOR": "env",
            "SMOLVM_ROLLOUT_POLICY": "env-policy",
        }
        with mock.patch.dict(os.environ, environment, clear=True):
            client = RolloutClient(fork_env_path=assignment)
        assert client.executor == "env"
        assert client.bearer_token == "env-token"
        assert client.lease_policy == "env-policy"


def test_rollout_incomplete_lease_assignment_fails_closed():
    with tempfile.TemporaryDirectory() as directory:
        assignment = Path(directory) / "fork-env"
        assignment.write_text("SMOLVM_ROLLOUT_TOKEN=secret\n", encoding="utf-8")
        with mock.patch.dict(os.environ, {}, clear=True):
            try:
                RolloutClient(fork_env_path=assignment)
                raise AssertionError("should reject an incomplete assignment")
            except RuntimeError as error:
                assert "missing" in str(error)


def test_rollout_explicit_cohort_is_validated_and_encoded():
    client = RecordingRolloutClient()
    common = {
        "idempotency_key": "request-1",
        "policy": "policy-1",
        "prompts": ["hello"],
        "max_tokens": 8,
    }
    client.generate(
        **common,
        cohort_id="training-step-1",
        cohort_size=4,
        cohort_max_wait_ms=250,
    )
    assert client.recorded[2]["cohort"] == {
        "id": "training-step-1",
        "size": 4,
        "maxWaitMs": 250,
    }
    for invalid in (
        {"cohort_id": "training-step-1"},
        {"cohort_max_wait_ms": 250},
        {"cohort_id": "training-step-1", "cohort_size": 0},
    ):
        try:
            client.job(**common, **invalid)
            raise AssertionError("should reject an invalid cohort")
        except ValueError:
            pass


def test_rollout_automatic_cohorts_are_aligned_retry_stable_and_optional():
    environment = {
        "SMOLVM_FORK_BATCH_ID": "batch-1",
        "SMOLVM_FORK_BATCH_SIZE": "2",
    }
    common = {
        "policy": "policy-1",
        "prompts": ["hello"],
        "max_tokens": 8,
    }
    with mock.patch.dict(os.environ, environment, clear=True):
        first = RecordingRolloutClient()
        second = RecordingRolloutClient()
        first.generate(idempotency_key="learner-0-step-0", **common)
        second.generate(idempotency_key="learner-1-step-0", **common)
        first_cohort = first.recorded[2]["cohort"]
        assert first_cohort == second.recorded[2]["cohort"]
        assert first_cohort["size"] == 2
        assert first_cohort["maxWaitMs"] == 250

        first.generate(idempotency_key="learner-0-step-0", **common)
        assert first.recorded[2]["cohort"] == first_cohort
        first.generate(idempotency_key="learner-0-step-1", **common)
        second.generate(idempotency_key="learner-1-step-1", **common)
        assert first.recorded[2]["cohort"] == second.recorded[2]["cohort"]
        assert first.recorded[2]["cohort"] != first_cohort

        disabled = RecordingRolloutClient(auto_fork_cohort=False)
        disabled.generate(idempotency_key="uncoordinated", **common)
        assert "cohort" not in disabled.recorded[2]


def test_rollout_automatic_cohorts_partition_large_batches():
    common = {
        "policy": "policy-1",
        "prompts": ["hello"],
        "max_tokens": 8,
    }
    cohorts = []
    for index in (0, 255, 256, 299):
        environment = {
            "SMOLVM_FORK_BATCH_ID": "batch-large",
            "SMOLVM_FORK_BATCH_SIZE": "300",
            "SMOLVM_FORK_INDEX": str(index),
        }
        with mock.patch.dict(os.environ, environment, clear=True):
            client = RecordingRolloutClient()
            client.generate(idempotency_key=f"learner-{index}", **common)
            cohorts.append(client.recorded[2]["cohort"])
    assert cohorts[0] == cohorts[1]
    assert cohorts[0]["size"] == 256
    assert cohorts[2] == cohorts[3]
    assert cohorts[2]["size"] == 44
    assert cohorts[0]["id"] != cohorts[2]["id"]


if __name__ == "__main__":
    import traceback

    fns = [v for k, v in sorted(globals().items()) if k.startswith("test_") and callable(v)]
    passed = failed = 0
    for fn in fns:
        try:
            fn()
            passed += 1
            print(f"  ok {fn.__name__}")
        except Exception:  # noqa: BLE001
            failed += 1
            print(f"  FAIL {fn.__name__}")
            traceback.print_exc()
    print(f"\n{passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)
