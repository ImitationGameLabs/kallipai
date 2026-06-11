"""Daemon lifecycle management inside Harbor containers.

Manages starting, health-checking, and stopping the just-agent-daemon
process that must run inside the container alongside the CLI tools.

The exec methods (``exec_as_root``, ``exec_as_agent``) belong to the
Harbor ``BaseInstalledAgent`` adapter instance, not to this class.
The adapter passes itself to each call so this helper can drive the
container operations without inheriting from Harbor's base classes.
"""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any, Protocol

# Canonical installation paths inside the container.
INSTALL_DIR = Path("/opt/just-agent")
BIN_DIR = INSTALL_DIR / "bin"
DAEMON_BIN = BIN_DIR / "just-agent-daemon"
RUN_BIN = BIN_DIR / "just-agent-run"
PID_FILE = Path("/tmp/just-agent-daemon.pid")

# Logs are written under /logs/agent (bind-mounted to the host) so
# they survive after the container is destroyed.
LOGS_DIR = Path("/logs/agent")
DAEMON_LOG = LOGS_DIR / "daemon.log"
RUN_LOG = LOGS_DIR / "run.log"


class _ExecContext(Protocol):
    """Protocol for Harbor's exec methods.

    Matches the ``BaseInstalledAgent`` interface:
    ``exec_as_root(environment, command, *, env, cwd, timeout_sec)``
    """

    async def exec_as_root(
        self,
        environment: Any,
        command: str,
        *,
        env: dict[str, str] | None = None,
        cwd: str | None = None,
        timeout_sec: int | None = None,
    ) -> Any: ...

    async def exec_as_agent(
        self,
        environment: Any,
        command: str,
        *,
        env: dict[str, str] | None = None,
        cwd: str | None = None,
        timeout_sec: int | None = None,
    ) -> Any: ...


class DaemonNotReadyError(Exception):
    """Raised when the daemon fails to become healthy within the timeout."""


class DaemonManager:
    """Manages the just-agent-daemon process lifecycle inside a Harbor container.

    The daemon must be running before ``just-agent-run`` can be invoked,
    because the runner connects to it over HTTP.  This helper starts the
    daemon in the background, polls for readiness, and provides cleanup.
    """

    def __init__(self) -> None:
        self._operator_token: str | None = None

    def set_operator_token(self, token: str) -> None:
        """Store the operator token used for health checks and client auth."""
        self._operator_token = token

    @property
    def operator_token(self) -> str:
        """Return the stored operator token, raising if unset."""
        if not self._operator_token:
            raise RuntimeError("operator token not set")
        return self._operator_token

    async def start(
        self,
        ctx: _ExecContext,
        environment: Any,
        env: dict[str, str],
    ) -> None:
        """Start the daemon as a background process inside the container.

        Uses ``nohup`` to detach the daemon from the exec session so it
        keeps running after the ``exec_as_agent`` call returns.

        Environment variables are passed via Harbor's ``env=`` parameter,
        which translates to ``docker compose exec -e KEY=VALUE`` flags.
        The daemon inherits these vars from the shell started by
        ``docker compose exec``.

        Args:
            ctx: The adapter instance (has ``exec_as_agent``).
            environment: Harbor ``BaseEnvironment`` for exec operations.
            env: Environment variables forwarded to the daemon process.
                 Must include ``JUST_AGENT_OPERATOR_TOKEN``.
        """
        # Ensure the log directory exists (it may not yet when the
        # bind mount is first set up).
        cmd = f"mkdir -p {LOGS_DIR}"

        # Launch daemon in background with nohup, write PID file.
        cmd += (
            f" && nohup {DAEMON_BIN}"
            f" --listen-addr 127.0.0.1:3000"
            f" --advertise-url http://127.0.0.1:3000"
            f" > {DAEMON_LOG} 2>&1 &"
            f" echo $! > {PID_FILE}"
        )
        await ctx.exec_as_agent(environment, command=cmd, env=env)

    async def wait_ready(self, ctx: _ExecContext, environment: Any) -> None:
        """Wait for the daemon to become ready.

        Currently uses a fixed sleep as a simple heuristic.  Harbor's
        minimal container images (e.g. ``ubuntu:24.04``) do not ship
        with ``curl`` or ``wget``, making HTTP-based health checks
        impractical without installing extra packages.
        """
        await asyncio.sleep(3)

    async def stop(self, ctx: _ExecContext, environment: Any) -> None:
        """Best-effort stop of the daemon process via its PID file.

        Not invoked during normal Harbor benchmarking — containers are
        ephemeral and destroyed after each task.  Kept as a utility for
        potential future use if Harbor adds a teardown lifecycle hook.
        """
        cmd = f"if [ -f {PID_FILE} ]; then kill $(cat {PID_FILE}) 2>/dev/null; fi"
        try:
            await ctx.exec_as_root(environment, command=cmd)
        except Exception:
            pass  # Best-effort; ignore errors during cleanup.

