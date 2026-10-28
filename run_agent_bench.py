import os
import tomllib
import subprocess
from pathlib import Path

path = Path.home() / ".kroma/config.toml"
with path.open("rb") as f:
    profile = tomllib.load(f)["profiles"]["deepseek"]

env = os.environ.copy()
env.update({
    "KROMA_BASE_URL": profile["base_url"],
    "KROMA_MODEL": profile["model"],
    "KROMA_API_KEY": profile["api_key"],
    "KROMA_DEEPSEEK_THINKING": "high",
})

subprocess.run(
    [
        "python3",
        "benchmarks/agent_task_suite.py",
        "--run",
        "--output",
        "/tmp/kroma-vs-pi.json",
    ],
    env=env,
    check=True,
)
