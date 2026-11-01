"""Installed Codex config/read contract; synthetic homes, no inference requests.

python3 tests/support/managed_launch_config.py target/debug/usage /path/to/codex
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from native_codex_context import Client, write


def main():
    usage, codex = map(lambda p: str(Path(p).resolve()), sys.argv[1:])
    with tempfile.TemporaryDirectory(prefix="usage-managed-config-") as directory:
        root = Path(directory).resolve()
        user = root / "user"
        project = user / "project"
        project.mkdir(parents=True)
        base = user / ".codex"
        original = ('model="official-model"\nmodel_provider="openai"\n'
                    'model_reasoning_effort="high"\ncheck_for_update_on_startup=false\n'
                    '[tui]\nnotifications=false\n'
                    '[mcp_servers.inherited]\ncommand="true"\nenabled=false\n'
                    f'[projects.{json.dumps(str(user))}]\ntrust_level="trusted"\n')
        write(base / "config.toml", original)
        write(project / ".codex/config.toml", 'model="project-model"\nmodel_provider="openai"\nmodel_reasoning_effort="high"\n')
        env = os.environ.copy()
        for key in list(env):
            if key.startswith(("USAGE_", "CODEX_")) or key in ["OPENAI_API_KEY", "OPENAI_BASE_URL", "CHATGPT_BASE_URL"]:
                env.pop(key)
        env.update(HOME=str(user), CODEX_HOME=str(base),
                   USAGE_CODEX_DIR=str(root / "manager"), USAGE_CODEX_BASE_HOME=str(base),
                   USAGE_PLUGIN_DIR=str(root / "plugins"), XDG_DATA_HOME=str(root / "data"),
                   XDG_CONFIG_HOME=str(root / "config"), USAGE_GATEWAY="off",
                   USAGE_LOG="off", USAGE_LOG_DIR=str(root / "logs"))
        for name in ["A", "B"]:
            result = subprocess.run([usage, "codex", "add", name, "--base-url",
                                     f"http://127.0.0.1:9/{name}/v1", "--model", f"managed-{name}",
                                     "--effort", "low", "--key-stdin"], env=env, cwd=user,
                                    input=f"synthetic-{name}\n", text=True, capture_output=True, timeout=20)
            assert result.returncode == 0, result.stderr
        clients = []
        try:
            # Keep both processes alive: same cwd and global config, independent routes.
            for name, cwd, extra in [("A", user, []), ("B", user, []),
                                     ("A", project, []), ("A", project, ["-c", 'model="chosen"'])]:
                client = Client([usage, "codex", "run", name, "--cwd", str(cwd),
                                 "--binary", codex, "--", *extra, "app-server", "--listen", "stdio://"],
                                env, cwd, root / f"{len(clients)}.stderr")
                clients.append(client)
                config = client.rpc("config/read", {"cwd": str(cwd)})["config"]
                assert config["model_provider"] == "usage_byok", config
                assert config["model"] == ("chosen" if extra else f"managed-{name}"), config
                assert config["model_reasoning_effort"] == "low", config
                provider = config["model_providers"]["usage_byok"]
                assert provider["base_url"] == f"http://127.0.0.1:9/{name}/v1", provider
                assert provider["env_key"] == "USAGE_CODEX_API_KEY", provider
                assert config["mcp_servers"]["inherited"]["enabled"] is False
                assert config["tui"]["notifications"] is False
            assert (base / "config.toml").read_text() == original
        finally:
            for client in clients:
                client.close()
        print("Native managed launch: home/project conflicts, concurrent accounts, model override and inherited preferences passed")


if __name__ == "__main__":
    main()
