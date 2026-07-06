# kallipai Harbor Integration

Binary/crate stem: `kallip`.

Harbor benchmarking adapter for [kallipai](../). Allows running kallip inside Harbor's container-based evaluation pipeline (e.g. terminal-bench).

## Prerequisites

- [Nix](https://nixos.org/) with flake support
- [Harbor](https://github.com/center-for-humans-and-machines/harbor) installed
- `podman-docker` (for podman compatibility with Harbor)

## Architecture

The adapter runs the full kallip stack **inside the Harbor container**:

```
install()  →  upload tarball → unpack → start daemon (background)
run()      →  kallip-run --prompt "$instruction" (connects to localhost daemon)
```

Both `kallip-daemon` and `kallip-run` run inside the container so the agent has direct filesystem access to benchmark task files.

## Setup

```bash
# Create venv + install adapter and harbor (tarball is built by harbor-test.sh)
./harbor-integration/setup-venv.sh

# Activate the environment (includes KALLIP_PACKAGE_PATH)
source harbor-integration/.venv/bin/activate
```

Or manually:

```bash
nix build .#kallip-tarball
python3 -m venv harbor-integration/.venv
source harbor-integration/.venv/bin/activate
pip install -e ./harbor-integration
export KALLIP_PACKAGE_PATH=./result/kallip-*-linux-x86_64.tar.gz
```

`harbor-test.sh` builds and injects **two** tarballs (both pinned by one `flake.lock`):

**`kallip-tarball`** (always installed):

| Binary          | Purpose                                       |
| --------------- | --------------------------------------------- |
| `kallip`        | Headless CLI for agent-to-agent orchestration |
| `kallip-daemon` | HTTP server hosting agent instances           |
| `kallip-run`    | One-shot runner for scripting/benchmarking    |

**`aifed-tarball`** (opt-in via `AIFED_PACKAGE_PATH`; aifed is kallip's intended file-editing dependency — runtime adoption pending — `x86_64-linux` only):

| Binary         | Purpose                                          |
| -------------- | ------------------------------------------------ |
| `aifed`        | AI-first file editor (read/edit/outline/lsp)     |
| `aifed-daemon` | Background LSP service (auto-spawned by `aifed`) |

`aifed`/`aifed-daemon` are symlinked into `/usr/local/bin` for bare-name lookup, and the adapter `command -v`-checks them at install time. If `AIFED_PACKAGE_PATH` is unset (e.g. cargo build fallback), the adapter skips aifed.

## Environment Variables

Set these on the **host** before running Harbor. They are forwarded into the container via Harbor's `ENV_VARS` mechanism.

| Variable                         | Required | Description                                                                       |
| -------------------------------- | -------- | --------------------------------------------------------------------------------- |
| `JUST_LLM_DEEPSEEK_API_KEY`      | Yes\*    | API key for DeepSeek provider                                                     |
| `JUST_LLM_OPENAI_COMPAT_API_KEY` | Yes\*    | API key for OpenAI-compatible provider                                            |
| `JUST_LLM_PROVIDER`              | No       | LLM backend: `deepseek` or `openai-compatible`. Auto-set from config `model_name` |
| `JUST_LLM_MODEL`                 | No       | Model identifier. Auto-set from config `model_name`                               |
| `KALLIP_OPERATOR_TOKEN`          | No       | Pre-set auth token; auto-generated if omitted                                     |
| `KALLIP_MAX_TOOL_ROUNDS`         | No       | Default max tool-call rounds per run                                              |

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
2. `install()` uploads the tarball, unpacks it to `/opt/kallip`, starts the daemon as a background process
3. `run()` invokes `kallip-run --prompt <instruction>` — it prints the final assistant reply to stdout and a completion hint (agent id + how to continue) to stderr. Pass `--verbose` to the runner for the full reasoning/tool log.
4. `kallip-run` exits with semantic codes: `0` success, `1` error, `2` max rounds, `3` cancelled, `4` budget exceeded
5. Harbor evaluates the result against the task's test suite

## Limitations

- **No `/health` endpoint**: Health checking uses an authenticated `GET /budget` request. A future daemon release may add an unauthenticated health endpoint.
