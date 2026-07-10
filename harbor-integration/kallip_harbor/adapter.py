"""Harbor ``BaseInstalledAgent`` adapter for kallip.

This module provides ``KallipAdapter``, a Harbor-compatible agent
that runs kallip inside benchmarking containers.  It manages the
full lifecycle:

1. **install()** — upload the tarball, unpack, start the daemon
2. **run()**     — invoke ``kallip-run`` with the task instruction
3. **populate_context_post_run()** — parse token usage from context.json

Usage::

    pip install -e ./harbor-integration
    harbor run --dataset terminal-bench@2.0 \\
        --agent-import-path "kallip_harbor:KallipAdapter" \\
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

from kallip_harbor.daemon import (
    PACKAGES,
    RUN_BIN,
    RUN_LOG,
    DaemonManager,
)


class KallipAdapter(BaseInstalledAgent):
    """Harbor adapter for kallip.

    Subclasses ``BaseInstalledAgent`` to integrate kallip into
    Harbor's container-based benchmarking pipeline.  The daemon runs
    inside the container, and ``kallip-run`` is invoked per task.
    """

    SUPPORTS_ATIF = False

    @staticmethod
    def name() -> str:
        return "kallip"

    def version(self) -> str | None:
        """Agent version — auto-detected by Harbor via get_version_command()."""
        return None

    # -- Declarative CLI flags for kallip-run --
    CLI_FLAGS: list[CliFlag] = [
        CliFlag(
            "max_rounds",
            cli="--max-rounds",
            type="int",
            env_fallback="KALLIP_MAX_TOOL_ROUNDS",
        ),
    ]

    # -- Declarative environment variables --
    # These are resolved from Harbor kwargs / host env / defaults, and
    # forwarded into the container for the daemon and runner.
    # Note: API keys are provider-specific, matching kallip's design:
    #   deepseek           → KALLIP_LLM_DEEPSEEK_API_KEY
    #   openai-compatible  → KALLIP_LLM_OPENAI_COMPAT_API_KEY
    ENV_VARS: list[EnvVar] = [
        EnvVar(
            "llm_provider",
            env="KALLIP_LLM_PROVIDER",
            type="str",
            env_fallback="KALLIP_LLM_PROVIDER",
        ),
        EnvVar(
            "llm_model",
            env="KALLIP_LLM_MODEL",
            type="str",
            env_fallback="KALLIP_LLM_MODEL",
        ),
        EnvVar(
            "llm_deepseek_api_key",
            env="KALLIP_LLM_DEEPSEEK_API_KEY",
            type="str",
            env_fallback="KALLIP_LLM_DEEPSEEK_API_KEY",
        ),
        EnvVar(
            "llm_openai_compat_api_key",
            env="KALLIP_LLM_OPENAI_COMPAT_API_KEY",
            type="str",
            env_fallback="KALLIP_LLM_OPENAI_COMPAT_API_KEY",
        ),
        EnvVar(
            "operator_token",
            env="KALLIP_OPERATOR_TOKEN",
            type="str",
            env_fallback="KALLIP_OPERATOR_TOKEN",
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
        """Install kallip (+ aifed) binaries and start the daemon."""
        # 1. CA certificates for reqwest TLS verification (installed once).
        await self.exec_as_root(
            environment,
            command="apt-get update -qq && apt-get install -y -qq ca-certificates",
        )

        # 2. Deploy each package: upload -> unpack under target -> chmod.
        #    on_path packages are symlinked into /usr/local/bin so the agent
        #    can invoke them by bare name (aifed auto-spawns aifed-daemon too).
        #    Optional packages (default=None) are skipped when their env var is unset.
        deployed_path_bins: list[str] = []
        for pkg in PACKAGES:
            src = os.environ.get(pkg.env)
            if src is None and pkg.default is None:
                self.logger.info(
                    "skipping optional package %s (%s unset)",
                    pkg.target.name,
                    pkg.env,
                )
                continue
            src_path = Path(src) if src is not None else pkg.default
            tmp = f"/tmp/{pkg.target.name}.tar.gz"

            await environment.upload_file(source_path=src_path, target_path=tmp)

            parts = [
                f"mkdir -p {pkg.target}",
                f"tar xzf {tmp} -C {pkg.target}",
                f"chmod +x {pkg.target}/bin/*",
            ]
            if pkg.on_path:
                parts.extend(
                    f"ln -sf {pkg.target}/bin/{b} /usr/local/bin/{b}"
                    for b in pkg.bins
                )
            parts.append(f"rm {tmp}")
            await self.exec_as_root(
                environment,
                command=" && ".join(parts),
            )
            if pkg.on_path:
                deployed_path_bins.extend(pkg.bins)

        #    Self-check: assert on_path bins are reachable by bare name, so a
        #    broken symlink or tarball-layout mismatch fails loudly. The agent
        #    may not otherwise exercise aifed, so the wiring must self-verify.
        if deployed_path_bins:
            await self.exec_as_root(
                environment,
                command=" && ".join(f"command -v {b}" for b in deployed_path_bins),
            )

        # 3. Resolve the operator token — use the host-provided value if set,
        #    otherwise generate a fresh random UUID per trial.
        env = self.resolve_env_vars()
        self._operator_token = env.get("KALLIP_OPERATOR_TOKEN") or str(uuid.uuid4())
        env["KALLIP_OPERATOR_TOKEN"] = self._operator_token

        # 4. Apply Harbor's --model if provided.
        #    Convention: --model "provider/model-name"
        #    Maps to KALLIP_LLM_PROVIDER + KALLIP_LLM_MODEL.
        if self.model_name and "/" in self.model_name:
            provider, model = self.model_name.split("/", 1)
            env.setdefault("KALLIP_LLM_PROVIDER", provider)
            env.setdefault("KALLIP_LLM_MODEL", model)

        # Persist daemon data under Harbor's bind-mounted /logs/agent/ so it
        # survives on the host at <trial_dir>/agent/ for post-run inspection.
        env.setdefault("KALLIP_DATA_DIR", "/logs/agent")

        # Harbor runs inside ephemeral containers; policy gates are not needed.
        env.setdefault("KALLIP_POLICY_PRESET", "allow-all")

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
        """Execute a task via kallip-run."""
        task = shlex.quote(instruction)
        flags = self.build_cli_flags()

        run_env = {
            "KALLIP_DAEMON_URL": "http://127.0.0.1:3000",
            "KALLIP_AUTH_TOKEN": self._operator_token,
            "SSL_CERT_FILE": "/etc/ssl/certs/ca-certificates.crt",
        }

        await self.exec_as_agent(
            environment,
            command=f"{RUN_BIN} {flags} --prompt {task} >{RUN_LOG} 2>&1",
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
        under ``KALLIP_DATA_DIR/kallip/agents/{uuid}/``.
        Since ``cumulative_usage`` is per-agent (not aggregated), we
        must sum across all agent directories.

        With ``KALLIP_DATA_DIR=/logs/agent`` (bind-mounted to
        ``self.logs_dir`` on the host), the data directory is at:
            self.logs_dir / "kallip" / "agents"
        """
        agents_dir = self.logs_dir / "kallip" / "agents"
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
