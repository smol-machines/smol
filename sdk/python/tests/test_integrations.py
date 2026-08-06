import os
import subprocess
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))
PYTHON_SOURCE = str(Path(__file__).resolve().parents[1] / "python")


class IntegrationBoundaryTests(unittest.TestCase):
    def test_core_import_does_not_load_or_export_framework_adapters(self):
        script = """
import sys
import smol

assert "smol.integrations" not in sys.modules
for name in (
    "UnslothVllmExecutor",
    "add_transformers_forkpoint",
    "peft_lora_tensors",
    "publish_peft_adapter",
):
    assert not hasattr(smol, name), name
"""
        subprocess.run(
            [sys.executable, "-c", script],
            check=True,
            env={**os.environ, "PYTHONPATH": PYTHON_SOURCE},
        )


if __name__ == "__main__":
    unittest.main()
