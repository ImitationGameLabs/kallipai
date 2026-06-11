# just-agent Harbor Integration

Harbor benchmarking adapter for [just-agent](../).  Allows running just-agent inside Harbor's container-based evaluation pipeline (e.g. terminal-bench).

## Prerequisites

- [Nix](https://nixos.org/) with flake support
- [Harbor](https://github.com/center-for-humans-and-machines/harbor) installed
- `podman-docker` (for podman compatibility with Harbor)

## Architecture

The adapter runs the full just-agent stack **inside the Harbor container**:

```
install()  →  upload tarball → unpack → start daemon (background)
run()      →  just-agent-run "$instruction" (connects to localhost daemon)
```

Both `just-agent-daemon` and `just-agent-run` run inside the container so the agent has direct filesystem access to benchmark task files.

## Setup

```bash
# Build tarball + create venv + install adapter and harbor
./harbor-integration/setup.sh

# Activate the environment (includes JUST_AGENT_PACKAGE_PATH)
source harbor-integration/.venv/bin/activate
```

Or manually:

```bash
nix build .#just-agent-tarball
python3 -m venv harbor-integration/.venv
source harbor-integration/.venv/bin/activate
pip install -e ./harbor-integration
export JUST_AGENT_PACKAGE_PATH=./result/just-agent-*-linux-x86_64.tar.gz
```

The tarball contains:

| Binary              | Purpose                                       |
| ------------------- | --------------------------------------------- |
| `just-agent`        | Headless CLI for agent-to-agent orchestration |
| `just-agent-daemon` | HTTP server hosting agent instances           |
| `just-agent-run`    | One-shot runner for scripting/benchmarking    |

## Environment Variables

Set these on the **host** before running Harbor.  They are forwarded into the container via Harbor's `ENV_VARS` mechanism.

| Variable                        | Required | Description                                             |
| ------------------------------- | -------- | ------------------------------------------------------- |
| `JUST_LLM_DEEPSEEK_API_KEY`    | Yes*     | API key for DeepSeek provider                           |
| `JUST_LLM_OPENAI_COMPAT_API_KEY` | Yes*   | API key for OpenAI-compatible provider                  |
| `JUST_LLM_PROVIDER`             | No       | LLM backend: `deepseek` or `openai-compatible`. Auto-set from config `model_name` |
| `JUST_LLM_MODEL`                | No       | Model identifier. Auto-set from config `model_name`     |
| `JUST_AGENT_OPERATOR_TOKEN`     | No       | Pre-set auth token; auto-generated if omitted           |
| `JUST_AGENT_MAX_TOOL_ROUNDS`    | No       | Default max tool-call rounds per run                    |

\* Set the key matching your provider.

## Running

Pre-built config files are in `harbor-integration/configs/`. Each config encodes the agent, model, dataset, and output directory so you don't need to pass them as CLI flags.

```bash
export JUST_LLM_DEEPSEEK_API_KEY=<your-key>

# Quick verification (hello-world)
./harbor-integration/harbor-test.sh

# Full terminal-bench evaluation
./harbor-integration/harbor-test.sh terminal-bench-2

# Custom config
./harbor-integration/harbor-test.sh --config path/to/custom.yaml
```

## How It Works

1. Harbor creates a container for the benchmark task
2. `install()` uploads the tarball, unpacks it to `/opt/just-agent`, starts the daemon as a background process
3. `run()` invokes `just-agent-run` with the task instruction — it streams progress to stderr and prints the final result to stdout
4. `just-agent-run` exits with semantic codes: `0` success, `1` error, `2` max rounds, `3` cancelled, `4` budget exceeded
5. Harbor evaluates the result against the task's test suite

## Limitations

- **No structured metrics**: `populate_context_post_run()` is currently a no-op. Token counts and cost are not reported back to Harbor.
- **No `/health` endpoint**: Health checking uses an authenticated `GET /budget` request. A future daemon release may add an unauthenticated health endpoint.
