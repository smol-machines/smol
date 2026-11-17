"""mini-swe-agent environment on a real local machine.

mini-swe-agent's own DockerEnvironment tests, ported, plus an agent run.
Opt-in, since each test boots a microVM:

SMOL_RUN_LOCAL_E2E=1 PYTHONPATH=sdk/python/python python -m pytest \
    sdk/python/tests/test_mini_swe_agent_e2e.py
"""

from __future__ import annotations

import os
import platform
from unittest.mock import patch

import pytest

pytest.importorskip("minisweagent")

from minisweagent.agents.default import DefaultAgent
from minisweagent.environments import get_environment
from minisweagent.models.test_models import DeterministicModel, make_output
from smol import ConnectOptions, Machine
from smol.mini_swe_agent import SmolEnvironment, SmolEnvironmentConfig

pytestmark = pytest.mark.skipif(
    os.environ.get("SMOL_RUN_LOCAL_E2E") != "1", reason="local VM opt-in"
)

IMAGE = "python:3.12-slim"


def test_config_defaults_match_docker_environment():
    config = SmolEnvironmentConfig(image=IMAGE)
    assert (config.cwd, config.env, config.forward_env, config.timeout) == ("/", {}, [], 30)


@pytest.mark.parametrize(
    ("config", "host_env", "command", "cwd", "returncode", "output"),
    [
        pytest.param({}, {}, "echo 'hello world'", "", 0, "hello world\n", id="basic"),
        pytest.param(
            {"env": {"TEST_VAR": "test_value", "ANOTHER_VAR": "another_value"}},
            {},
            "echo $TEST_VAR $ANOTHER_VAR",
            "",
            0,
            "test_value another_value\n",
            id="set-env",
        ),
        pytest.param(
            {"forward_env": ["HOST_VAR", "ANOTHER_HOST_VAR"]},
            {"HOST_VAR": "host_value", "ANOTHER_HOST_VAR": "another_host_value"},
            "echo $HOST_VAR $ANOTHER_HOST_VAR",
            "",
            0,
            "host_value another_host_value\n",
            id="forward-env",
        ),
        pytest.param(
            {"forward_env": ["NONEXISTENT_VAR"]},
            {},
            'echo "[$NONEXISTENT_VAR]"',
            "",
            0,
            "[]\n",
            id="forward-unset",
        ),
        pytest.param(
            {"env": {"SET_VAR": "from_config"}, "forward_env": ["HOST_VAR"]},
            {"HOST_VAR": "from_host"},
            "echo $SET_VAR $HOST_VAR",
            "",
            0,
            "from_config from_host\n",
            id="env-and-forward",
        ),
        pytest.param(
            {"env": {"CONFLICT_VAR": "from_config"}, "forward_env": ["CONFLICT_VAR"]},
            {"CONFLICT_VAR": "from_host"},
            "echo $CONFLICT_VAR",
            "",
            0,
            "from_config\n",
            id="env-wins-over-forward",
        ),
        pytest.param({"cwd": "/tmp"}, {}, "pwd", "", 0, "/tmp\n", id="config-cwd"),
        pytest.param({"cwd": "/"}, {}, "pwd", "/tmp", 0, "/tmp\n", id="cwd-argument-wins"),
        pytest.param({}, {}, "exit 42", "", 42, "", id="exit-code"),
    ],
)
def test_execute_behaves_like_docker_environment(
    config, host_env, command, cwd, returncode, output
):
    with patch.dict(os.environ, host_env):
        env = SmolEnvironment(image=IMAGE, **config)
        try:
            result = env.execute({"command": command}, cwd=cwd)
        finally:
            env.cleanup()
    assert (result["returncode"], result["output"], result["exception_info"]) == (
        returncode,
        output,
        "",
    )


def test_cleanup_deletes_the_machine():
    env = SmolEnvironment(image=IMAGE)
    name = env.machine.name
    local = ConnectOptions(target="local")
    assert name in [m.name for m in Machine.list(local)]
    env.cleanup()
    env.cleanup()
    assert name not in [m.name for m in Machine.list(local)]


def test_stderr_and_timeouts_come_back_as_docker_environment_returns_them():
    env = SmolEnvironment(image=IMAGE, timeout=2)
    try:
        merged = env.execute({"command": "echo out1; echo err1 >&2; echo out2"})
        timed_out = env.execute({"command": "echo partial; sleep 10"})
        exited_124 = env.execute({"command": "exit 124"})
    finally:
        env.cleanup()
    # stderr interleaves with stdout, as DockerEnvironment's stderr=STDOUT gives.
    assert merged["output"] == "out1\nerr1\nout2\n"
    # A killed command is an error with what it printed so far; its own 124 is not.
    assert (timed_out["returncode"], timed_out["output"]) == (-1, "partial\n")
    assert "timed out after 2 seconds" in timed_out["exception_info"]
    assert (exited_124["returncode"], exited_124["exception_info"]) == (124, "")


def test_an_agent_loaded_by_dotted_path_fixes_a_bug_and_submits():
    env = get_environment(
        {
            "environment_class": "smol.mini_swe_agent.SmolEnvironment",
            "image": IMAGE,
            "cwd": "/testbed",
        }
    )
    setup = "mkdir -p /testbed && printf 'def add(a, b):\\n    return a - b\\n' > /testbed/calc.py"
    assert env.execute({"command": setup}, cwd="/")["returncode"] == 0
    steps = [
        "python3 -B -c 'import calc; print(calc.add(2, 3))'",
        "sed -i 's/a - b/a + b/' calc.py && python3 -B -c 'import calc; print(calc.add(2, 3))'",
        "echo COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT && cat calc.py",
    ]
    model = DeterministicModel(
        outputs=[make_output("step", [{"command": c}], cost=0.0) for c in steps]
    )
    agent = DefaultAgent(
        model, env, system_template="You fix bugs.", instance_template="{{task}}", step_limit=5
    )
    try:
        result = agent.run("Make add(2, 3) return 5.")
    finally:
        env.cleanup()
    observations = [m["content"] for m in agent.messages if m["role"] == "user"][1:]
    assert observations == [
        "<returncode>0</returncode>\n<output>\n-1\n</output>",
        "<returncode>0</returncode>\n<output>\n5\n</output>",
    ]
    assert result["exit_status"] == "Submitted"
    assert result["submission"] == "def add(a, b):\n    return a + b\n"


@pytest.mark.skipif(
    platform.system() != "Darwin" or platform.machine() != "arm64",
    reason="Rosetta is Apple silicon only",
)
def test_rosetta_runs_an_x86_64_image_on_apple_silicon():
    env = SmolEnvironment(image=IMAGE, rosetta=True)
    try:
        result = env.execute(
            {"command": "uname -m; python3 -c 'import platform; print(platform.machine())'"}
        )
    finally:
        env.cleanup()
    assert result["output"] == "x86_64\nx86_64\n"
