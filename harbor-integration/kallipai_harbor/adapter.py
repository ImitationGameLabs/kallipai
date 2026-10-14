"""Harbor ``BaseInstalledAgent`` adapter for kallipai.

This module provides ``KallipaiAdapter``, a Harbor-compatible agent
that runs kallipai inside benchmarking containers.  It manages the
full lifecycle:

1. **install()** — upload the tarball, unpack, start the tagma
2. **run()**     — invoke ``kallipai-run`` with the task instruction
3. **populate_context_post_run()** — parse token usage from context.json

Usage::

    pip install -e ./harbor-integration
    harbor run --dataset terminal-bench@2.0 \\
        --agent-import-path "kallipai_harbor:KallipaiAdapter"

The provider, model, and API key come from
``harbor-integration/.localprofile.toml`` (see the README), which the
adapter ships into the container as the tagma's config file.
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

from kallipai_harbor.profiles import resolve_local_profile
from kallipai_harbor.tagma import (
    PACKAGES,
    agents_dir as agents_dir_fn,
    CONFIG_HOME,
    PROFILE_STAGING,
    RUN_BIN,
    RUN_LOG,
    TagmaManager,
)


class KallipaiAdapter(BaseInstalledAgent):
    """Harbor adapter for kallipai.

    Subclasses ``BaseInstalledAgent`` to integrate kallipai into
    Harbor's container-based benchmarking pipeline.  The tagma runs
    inside the container, and ``kallipai-run`` is invoked per task.
    """

    SUPPORTS_ATIF = False

    @staticmethod
    def name() -> str:
        return "kallipai"

    def version(self) -> str | None:
        """Agent version — auto-detected by Harbor via get_version_command()."""
        return None

    # -- Declarative CLI flags for kallipai-run --
    # None today. kallipai-run posts the prompt to the tagma's single root agent
    # and no longer takes spawn-time flags; a round cap, if ever wanted, would be
    # set on the tagma via KALLIPAI_MAX_TOOL_ROUNDS (read at root creation), not
    # passed per-run. Benchmark configs intentionally leave rounds uncapped so
    # complex tasks are not cut off mid-completion.
    CLI_FLAGS: list[CliFlag] = []

    # -- Declarative environment variables --
    # These are resolved from Harbor kwargs / host env / defaults, and
    # forwarded into the container for the tagma and runner. Model access
    # is deliberately absent: install() ships ``.localprofile.toml`` in as
    # the tagma's config file instead, so no API key rides the environment.
    ENV_VARS: list[EnvVar] = [
        EnvVar(
            "operator_token",
            env="KALLIPAI_OPERATOR_TOKEN",
            type="str",
            env_fallback="KALLIPAI_OPERATOR_TOKEN",
        ),
    ]

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        self._tagma = TagmaManager()
        self._operator_token: str = ""

    # ------------------------------------------------------------------
    # install
    # ------------------------------------------------------------------

    async def install(self, environment: BaseEnvironment) -> None:
        """Install kallipai (+ aifed) binaries and start the tagma."""
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
        self._operator_token = env.get("KALLIPAI_OPERATOR_TOKEN") or str(uuid.uuid4())
        env["KALLIPAI_OPERATOR_TOKEN"] = self._operator_token

        # Ship the operator's LLM configuration in as a file: the tagma
        # reads it from its config home, so the model settings have one
        # source of truth and the key stays out of the environment.
        await environment.upload_file(
            source_path=resolve_local_profile(), target_path=PROFILE_STAGING
        )

        # Slug boot: name this trial's instance (install-time generated),
        # and anchor the XDG homes at Harbor's bind-mounted /logs/agent so
        # the derived data root (/logs/agent/kallipai/tagmata/<slug>) and
        # the logs land on the host at <trial_dir>/agent/ for post-run
        # inspection; the config home is the sibling holding the delivered
        # profiles.toml. The env reaches the tagma child only - Harbor's own
        # process is untouched.
        self._slug = f"bench-{uuid.uuid4().hex[:8]}"
        env["KALLIPAI_TAGMA_SLUG"] = self._slug
        env["XDG_DATA_HOME"] = str(self.logs_dir)
        env["XDG_STATE_HOME"] = str(self.logs_dir)
        env["XDG_CONFIG_HOME"] = str(CONFIG_HOME)

        # Harbor runs inside ephemeral containers; policy gates are not needed.
        env.setdefault("KALLIPAI_POLICY_PRESET", "allow-all")

        # Nix-built binaries cannot find the system CA store after
        # patchelf rewrites the interpreter.  Point them at the FHS
        # standard location so reqwest (used by both tagma and runner)
        # can verify TLS certificates when calling LLM providers.
        env.setdefault("SSL_CERT_FILE", "/etc/ssl/certs/ca-certificates.crt")

        # 4. Start the tagma.
        self._tagma.set_operator_token(self._operator_token)
        await self._tagma.start(self, environment, env)

        # 5. Wait for the tagma to become healthy.
        await self._tagma.wait_ready(self, environment)

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
        """Execute a task via kallipai-run."""
        task = shlex.quote(instruction)
        flags = self.build_cli_flags()

        run_env = {
            "KALLIPAI_TAGMA_URL": "http://127.0.0.1:3000",
            "KALLIPAI_AUTH_TOKEN": self._operator_token,
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
        under ``<data root>/agents/{uuid}/``; the data root is the
        slug-derived tree this adapter named at install time
        (``tagma.agents_dir`` mirrors the Rust resolver). Since
        ``cumulative_usage`` is per-agent (not aggregated), we must sum
        across all agent directories.
        """
        agents_dir = agents_dir_fn(self.logs_dir, self._slug)
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
