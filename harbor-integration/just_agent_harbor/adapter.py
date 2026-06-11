"""Harbor ``BaseInstalledAgent`` adapter for just-agent.

This module provides ``JustAgentAdapter``, a Harbor-compatible agent
that runs just-agent inside benchmarking containers.  It manages the
full lifecycle:

1. **install()** — upload the tarball, unpack, start the daemon
2. **run()**     — invoke ``just-agent-run`` with the task instruction
3. **populate_context_post_run()** — parse token usage from context.json

Usage::

    pip install -e ./harbor-integration
    harbor run --dataset terminal-bench@2.0 \\
        --agent-import-path "just_agent_harbor:JustAgentAdapter" \\
        --model "deepseek/deepseek-v4-flash"
"""

from __future__ import annotations

import json
import os
import shlex
import uuid
from pathlib import Path
from typing import Any

from harbor.agents.installed.base import (
    BaseInstalledAgent,
    CliFlag,
    EnvVar,
    with_prompt_template,
)
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

from just_agent_harbor.daemon import (
    BIN_DIR,
    INSTALL_DIR,
    LOGS_DIR,
    RUN_BIN,
    RUN_LOG,
    DaemonManager,
)

# Path to the tarball on the host machine.
# Override via environment variable for non-default locations.
DEFAULT_PACKAGE_PATH = Path("./just-agent-linux-x86_64.tar.gz")
PACKAGE_PATH_ENV = "JUST_AGENT_PACKAGE_PATH"


class JustAgentAdapter(BaseInstalledAgent):
    """Harbor adapter for just-agent.

    Subclasses ``BaseInstalledAgent`` to integrate just-agent into
    Harbor's container-based benchmarking pipeline.  The daemon runs
    inside the container, and ``just-agent-run`` is invoked per task.
    """

    SUPPORTS_ATIF = False

    @staticmethod
    def name() -> str:
        return "just-agent"

    def version(self) -> str | None:
        """Agent version — auto-detected by Harbor via get_version_command()."""
        return None

    # -- Declarative CLI flags for just-agent-run --
    CLI_FLAGS: list[CliFlag] = [
        CliFlag(
            "max_rounds",
            cli="--max-rounds",
            type="int",
            env_fallback="JUST_AGENT_MAX_TOOL_ROUNDS",
        ),
    ]

    # -- Declarative environment variables --
    # These are resolved from Harbor kwargs / host env / defaults, and
    # forwarded into the container for the daemon and runner.
    # Note: API keys are provider-specific, matching just-agent's design:
    #   deepseek           → JUST_LLM_DEEPSEEK_API_KEY
    #   openai-compatible  → JUST_LLM_OPENAI_COMPAT_API_KEY
    ENV_VARS: list[EnvVar] = [
        EnvVar(
            "llm_provider",
            env="JUST_LLM_PROVIDER",
            type="str",
            env_fallback="JUST_LLM_PROVIDER",
        ),
        EnvVar(
            "llm_model",
            env="JUST_LLM_MODEL",
            type="str",
            env_fallback="JUST_LLM_MODEL",
        ),
        EnvVar(
            "llm_deepseek_api_key",
            env="JUST_LLM_DEEPSEEK_API_KEY",
            type="str",
            env_fallback="JUST_LLM_DEEPSEEK_API_KEY",
        ),
        EnvVar(
            "llm_openai_compat_api_key",
            env="JUST_LLM_OPENAI_COMPAT_API_KEY",
            type="str",
            env_fallback="JUST_LLM_OPENAI_COMPAT_API_KEY",
        ),
        EnvVar(
            "operator_token",
            env="JUST_AGENT_OPERATOR_TOKEN",
            type="str",
            env_fallback="JUST_AGENT_OPERATOR_TOKEN",
        ),
    ]

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        self._daemon = DaemonManager()
        self._operator_token: str = ""

    # ------------------------------------------------------------------
    # install
    # ------------------------------------------------------------------

    async def install(self, environment: BaseEnvironment) -> None:
        """Install just-agent binaries and start the daemon inside the container."""
        # 1. Upload the tarball from the host.
        tar_path = Path(os.environ.get(PACKAGE_PATH_ENV, str(DEFAULT_PACKAGE_PATH)))
        await environment.upload_file(
            source_path=tar_path,
            target_path="/tmp/just-agent.tar.gz",
        )

        # 2. Install CA certificates (required by rustls-platform-verifier)
        #    and unpack the tarball.
        await self.exec_as_root(
            environment,
            command=(
                "apt-get update -qq && apt-get install -y -qq ca-certificates"
                f" && mkdir -p {INSTALL_DIR}"
                f" && tar xzf /tmp/just-agent.tar.gz -C {INSTALL_DIR}"
                f" && chmod +x {BIN_DIR}/*"
                f" && rm /tmp/just-agent.tar.gz"
            ),
        )

        # 3. Resolve the operator token (or fall back to a fixed placeholder).
        env = self.resolve_env_vars()
        self._operator_token = env.get("JUST_AGENT_OPERATOR_TOKEN") or "just-agent-operator-token"
        # self._operator_token = env.get("JUST_AGENT_OPERATOR_TOKEN") or str(
        #     uuid.uuid4()
        # )
        env["JUST_AGENT_OPERATOR_TOKEN"] = self._operator_token

        # 4. Apply Harbor's --model if provided.
        #    Convention: --model "provider/model-name"
        #    Maps to JUST_LLM_PROVIDER + JUST_LLM_MODEL.
        if self.model_name and "/" in self.model_name:
            provider, model = self.model_name.split("/", 1)
            env.setdefault("JUST_LLM_PROVIDER", provider)
            env.setdefault("JUST_LLM_MODEL", model)

        # Persist daemon data under Harbor's bind-mounted /logs/agent/ so it
        # survives on the host at <trial_dir>/agent/ for post-run inspection.
        env.setdefault("JUST_AGENT_DATA_DIR", "/logs/agent")

        # Harbor runs inside ephemeral containers; policy gates are not needed.
        env.setdefault("JUST_AGENT_POLICY_PRESET", "allow-all")

        # Nix-built binaries cannot find the system CA store after
        # patchelf rewrites the interpreter.  Point them at the FHS
        # standard location so reqwest (used by both daemon and runner)
        # can verify TLS certificates when calling LLM providers.
        env.setdefault("SSL_CERT_FILE", "/etc/ssl/certs/ca-certificates.crt")

        # 5. Start the daemon.
        self._daemon.set_operator_token(self._operator_token)
        await self._daemon.start(self, environment, env)

        # 6. Wait for the daemon to become healthy.
        await self._daemon.wait_ready(self, environment)

    # ------------------------------------------------------------------
    # run
    # ------------------------------------------------------------------

    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        """Execute a task via just-agent-run."""
        task = shlex.quote(instruction)
        flags = self.build_cli_flags()

        run_env = {
            "JUST_AGENT_DAEMON_URL": "http://127.0.0.1:3000",
            "JUST_AGENT_AUTH_TOKEN": self._operator_token,
            "SSL_CERT_FILE": "/etc/ssl/certs/ca-certificates.crt",
        }
        self.logger.info("run() env dict: %s", run_env)

        await self.exec_as_agent(
            environment,
            command=f"{RUN_BIN} {flags} {task} >{RUN_LOG} 2>&1",
            env=run_env,
        )

    # ------------------------------------------------------------------
    # version detection
    # ------------------------------------------------------------------

    def get_version_command(self) -> str | None:
        """Return the command Harbor uses to detect the agent version."""
        return f"{RUN_BIN} --version"

    def parse_version(self, stdout: str) -> str:
        """Extract version from clap's ``<bin> <version>`` output."""
        return stdout.strip().split()[-1].lstrip("v")

    # ------------------------------------------------------------------
    # post-run metrics
    # ------------------------------------------------------------------

    def populate_context_post_run(self, context: AgentContext) -> None:
        """Parse context.json files for token usage metrics.

        Each agent (root + subagents) persists its own ``context.json``
        under ``JUST_AGENT_DATA_DIR/just-agent/agents/{uuid}/``.
        Since ``cumulative_usage`` is per-agent (not aggregated), we
        must sum across all agent directories.

        With ``JUST_AGENT_DATA_DIR=/logs/agent`` (bind-mounted to
        ``self.logs_dir`` on the host), the data directory is at:
            self.logs_dir / "just-agent" / "agents"
        """
        agents_dir = self.logs_dir / "just-agent" / "agents"
        if not agents_dir.is_dir():
            return

        total_prompt = 0
        total_completion = 0
        total_cache = 0

        for context_file in agents_dir.glob("*/context.json"):
            try:
                data = json.loads(context_file.read_text(encoding="utf-8"))
                usage = data.get("cumulative_usage", {})
                total_prompt += usage.get("prompt_tokens", 0)
                total_completion += usage.get("completion_tokens", 0)
                total_cache += usage.get("cache_hit_tokens", 0)
            except (OSError, json.JSONDecodeError):
                self.logger.debug("failed to parse %s", context_file, exc_info=True)
                continue

        if total_prompt or total_completion:
            context.n_input_tokens = total_prompt
            context.n_output_tokens = total_completion
            context.n_cache_tokens = total_cache
