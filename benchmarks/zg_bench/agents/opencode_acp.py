from __future__ import annotations

import json
import shlex
from inspect import getfile
from pathlib import Path
from typing import Any

from harbor.agents.installed.acp import AcpAgent
from harbor.environments.base import BaseEnvironment

from ..github_proxy import GithubProxyMixin


class OpenCodeACP(GithubProxyMixin, AcpAgent):
    """Run OpenCode through its official ACP server with project config."""

    _ACP_PYTHON_PACKAGE = "agent-client-protocol==0.11.0"
    _LEGACY_SET_MODEL_CALL = """\
                        set_model_response = await conn.set_session_model(
                            model_id=candidate_model_id,
                            session_id=session.session_id,
                        )"""
    _COMPATIBLE_SET_MODEL_CALL = """\
                        if hasattr(conn, "set_config_option"):
                            set_model_response = await conn.set_config_option(
                                config_id="model",
                                value=candidate_model_id,
                                session_id=session.session_id,
                            )
                        else:
                            set_model_response = await conn.set_session_model(
                                model_id=candidate_model_id,
                                session_id=session.session_id,
                            )"""

    def __init__(
        self,
        *args: Any,
        opencode_config: dict[str, Any] | None = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(*args, **kwargs)
        self._opencode_config = dict(opencode_config or {})

    def _build_dependencies_command(self, kind: Any) -> str:
        fallback = super()._build_dependencies_command(kind).replace(
            "pip install agent-client-protocol",
            f"pip install {self._ACP_PYTHON_PACKAGE}",
        )
        if kind != "binary":
            return fallback

        runner_python = f"{self._RUNNER_VENV_PATH}/bin/python"
        runner_pip = f"{self._RUNNER_VENV_PATH}/bin/pip"
        return f"""
set -euo pipefail
if command -v python3 >/dev/null 2>&1 \
    && command -v curl >/dev/null 2>&1 \
    && command -v tar >/dev/null 2>&1 \
    && python3 -m venv --help >/dev/null 2>&1 \
    && python3 -c 'import ssl' >/dev/null 2>&1; then
  if [ ! -x {runner_python} ] \
      || ! {runner_python} -c 'import acp' >/dev/null 2>&1; then
    rm -rf {self._RUNNER_VENV_PATH}
    python3 -m venv {self._RUNNER_VENV_PATH}
    {runner_pip} install {self._ACP_PYTHON_PACKAGE}
  fi
else
{fallback}
fi
""".strip()

    @classmethod
    def _build_compatible_runner_source(cls) -> str:
        runner_path = Path(getfile(AcpAgent)).with_name("acp_runner.py")
        source = runner_path.read_text()
        if source.count(cls._LEGACY_SET_MODEL_CALL) != 1:
            raise RuntimeError(
                "Harbor's ACP runner model-selection code changed; "
                "review the OpenCode ACP compatibility adapter"
            )
        return source.replace(
            cls._LEGACY_SET_MODEL_CALL,
            cls._COMPATIBLE_SET_MODEL_CALL,
            1,
        )

    async def install(self, environment: BaseEnvironment) -> None:
        await super().install(environment)

        runner_path = self.logs_dir / "opencode-acp-runner.py"
        runner_path.write_text(self._build_compatible_runner_source())
        await environment.upload_file(
            source_path=runner_path,
            target_path=self._RUNNER_REMOTE_PATH,
        )
        await environment.exec(
            command=f"chmod +x {self._RUNNER_REMOTE_PATH}",
            user="root",
        )

    async def setup(self, environment: BaseEnvironment) -> None:
        await super().setup(environment)
        if not self._opencode_config:
            return

        config = json.dumps(self._opencode_config, indent=2, sort_keys=True)
        await self.exec_as_agent(
            environment,
            command=(
                "mkdir -p ~/.config/opencode && "
                f"printf '%s\\n' {shlex.quote(config)} "
                "> ~/.config/opencode/opencode.json"
            ),
        )
