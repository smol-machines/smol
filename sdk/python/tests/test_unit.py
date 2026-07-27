"""Pure-unit tests — no VM boot, no network. Mirrors the Node ``test/unit.ts``."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))

from smol.errors import ExecutionError, SmolError, wrap_native_error  # noqa: E402
from smol.transport import _cli_config_api_key, _encode_path, _native_config  # noqa: E402
from smol.types import ExecResult, MachineConfig, ResourceSpec  # noqa: E402


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
