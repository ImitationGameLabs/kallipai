# kallipai Harbor Integration

Binary/crate stem: `kallipai` (grandfathered CLI names: `kallip`, `kallipctl`).

Harbor benchmarking adapter for [kallipai](../). Allows running kallipai inside Harbor's container-based evaluation pipeline (e.g. terminal-bench).

## Prerequisites

- [Nix](https://nixos.org/) with flake support
- [Harbor](https://github.com/center-for-humans-and-machines/harbor) installed
- `podman-docker` (for podman compatibility with Harbor)

## Architecture

The adapter runs the full kallipai stack **inside the Harbor container**:

```text
install()  →  upload tarball → unpack → start tagma (background)
run()      →  kallipai-run --prompt "$instruction" (connects to localhost tagma)
```

Both `kallipai-tagma` and `kallipai-run` run inside the container so the agent has direct filesystem access to benchmark task files.

## Setup

```bash
# Create venv + install adapter and harbor (tarball is built by harbor-test.sh)
./harbor-integration/setup-venv.sh

# Activate the environment (includes KALLIPAI_PACKAGE_PATH)
source harbor-integration/.venv/bin/activate
```

Or manually:

```bash
nix build .#kallipai-tarball
python3 -m venv harbor-integration/.venv
source harbor-integration/.venv/bin/activate
pip install -e ./harbor-integration
export KALLIPAI_PACKAGE_PATH=./result/kallipai-*-linux-x86_64.tar.gz
```

`harbor-test.sh` builds and injects **two** tarballs (both pinned by one `flake.lock`):

**`kallipai-tarball`** (always installed):

| Binary           | Purpose                                       |
| --------------   | --------------------------------------------- |
| `kallip`         | Headless CLI for agent-to-agent orchestration |
| `kallipai-tagma` | HTTP server hosting agent instances           |
| `kallipai-run`   | One-shot runner for scripting/benchmarking    |

**`aifed-tarball`** (opt-in via `AIFED_PACKAGE_PATH`; aifed is kallipai's intended file-editing dependency — runtime adoption pending — `x86_64-linux` only):

| Binary         | Purpose                                          |
| -------------- | ------------------------------------------------ |
| `aifed`        | AI-first file editor (read/edit/outline/lsp)     |
| `aifed-daemon` | Background LSP service (auto-spawned by `aifed`) |

`aifed`/`aifed-daemon` are symlinked into `/usr/local/bin` for bare-name lookup, and the adapter `command -v`-checks them at install time. If `AIFED_PACKAGE_PATH` is unset (e.g. cargo build fallback), the adapter skips aifed.

## LLM configuration

Model access comes from the file the adapter stages into the container: it
copies `harbor-integration/.localprofile.toml` in, and the tagma reads it as
its own `profiles.toml`:

```bash
cp harbor-integration/localprofile.example.toml harbor-integration/.localprofile.toml
$EDITOR harbor-integration/.localprofile.toml
```

The file declares providers (`family`, `api_key`, optional `base_url`) and
one or more profile sets (`id`, `endpoint`, `model`, `max_context_window`);
the example carries the exact shape. A missing file fails the install before
the tagma starts, naming the file.

## Environment Variables

| Variable                  | Required | Description                                   |
| ------------------------- | -------- | --------------------------------------------- |
| `KALLIPAI_OPERATOR_TOKEN` | No       | Pre-set auth token; auto-generated if omitted |

Harbor's own `model_name` (from the config or `--model`) stays Harbor's
record of what it asked for; the model that actually runs is the one in the
profile file. Keep the two in step so benchmark results are labelled with
the model they used.

Note: tasks run uncapped (no `max_tool_rounds`); the agent runs until it
finishes or the tagma token budget is exhausted, so complex tasks are not cut
off mid-completion.

## Running

Pre-built config files are in `harbor-integration/configs/`. Each config encodes the agent, model, dataset, and output directory so you don't need to pass them as CLI flags.

```bash
# Configure harbor-integration/.localprofile.toml first (see above)

# Quick verification (hello-world)
./harbor-integration/harbor-test.sh

# Full terminal-bench evaluation
./harbor-integration/harbor-test.sh terminal-bench-2

# Custom config
./harbor-integration/harbor-test.sh --config path/to/custom.yaml
```

## How It Works

1. Harbor creates a container for the benchmark task
2. `install()` uploads the tarball, unpacks it to `/opt/kallipai`, starts the tagma as a background process
3. `run()` invokes `kallipai-run --prompt <instruction>` — it prints the final assistant reply to stdout and a completion hint (agent id + how to continue) to stderr. Pass `--verbose` to the runner for the full reasoning/tool log.
4. `kallipai-run` exits with semantic codes: `0` success, `1` error, `2` max rounds, `3` cancelled, `4` budget exceeded
5. Harbor evaluates the result against the task's test suite

## Limitations

- **No `/health` endpoint**: Health checking uses an authenticated `GET /budget` request. A future tagma release may add an unauthenticated health endpoint.
