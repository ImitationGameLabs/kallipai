# Docker image

`kallip-daemon` ships as a container image built with
`nixpkgs.dockerTools` — no Dockerfile. It shares the same crane-built workspace
as the `kallip-tarball` package, so the binaries inside are bit-identical to
a `nix build .#kallip-tarball` build. The image is **scratch-based**: glibc,
bash, the CA bundle, and every other runtime dependency come from the nix store
closure embedded in the image. Only `x86_64-linux` is published.

The recommended way to run it is [Arion](https://docs.hercules-ci.com/arion/) (a
Nix-native docker-compose), configured by `arion-compose.nix` at the repo root.
Three modes are switched by an env var:

| Mode     | Command                           | Image source                                      | Host nix store |
| -------- | --------------------------------- | ------------------------------------------------- | -------------- |
| **dev**  | `arion up -d` (default)           | `packages.default`, run via useHostStore          | shared (ro)    |
| **prod** | `KALLIP_ARION_MODE=prod arion up` | `packages.kallip-image` (pre-built)               | not shared     |
| **test** | `KALLIP_ARION_MODE=test arion up` | `packages.kallip-integration-tests`, useHostStore | shared (ro)    |

## Prerequisites

Arion and a Docker (or Podman with docker socket) daemon. On NixOS:

```nix
environment.systemPackages = [ pkgs.arion ];
virtualisation.docker.enable = true;   # or podman + dockerSocket
```

Copy `.env.example` to `.env` and fill in the LLM provider credentials. Arion
reads `.env` via `service.env_file`.

## Dev: `arion up` (default)

Dev mode skips the image bake entirely. `useHostStore` bind-mounts the host
`/nix/store` read-only into the container, and the daemon runs straight out of
the crane workspace (`packages.default`). So iterating is just:

```sh
# after editing Rust source:
nix build .#default        # rebuilds the workspace (incremental via crane)
arion up -d                # picks up the new binary
```

Arion resolves the same workspace path as `nix build .#default`, so there is no
store duplication.

## Production: `KALLIP_ARION_MODE=prod arion up`

```sh
KALLIP_ARION_MODE=prod arion up -d
arion logs -f
```

Arion builds `packages.kallip-image` (the flake's two-layer `buildImage`),
loads it, and runs it — one command, no manual `docker load`.

## Integration tests: `KALLIP_ARION_MODE=test arion up`

Test mode runs the workspace's integration tests (`[[test]]` targets) **inside
the container** to confirm the sandbox and shell backends behave correctly in
the containerized environment the daemon ships in. Today that covers the
`sandbox` suite (`crates/kallip-daemon/tests/sandbox/` — a scripted
end-to-end agent driving the real landlock + seccomp + mount-ns shell sandbox)
and the `exec` suite (`crates/kallip-shell/tests/exec.rs` — real `bash -c`
cwd/process-group behavior). Any `[[test]]` added later is picked up
automatically.

The test binaries are pre-built by Nix (`packages.kallip-integration-tests`):
every `[[test]]` artifact built via `cargo test --no-run`, plus the agent
binaries, merged into one closure. No build happens in the container; an
in-process wiremock stands in for the LLM, so no provider credentials or
external network are needed. The sandbox scenarios' scratch dirs live on a
`/testdata` tmpfs (outside the sandbox's baseline-writable set, so write-denial
assertions stay honest).

```sh
KALLIP_ARION_MODE=test arion up
arion ps -a          # exit code is the verdict (0 = all tests passed)
arion logs kallip
```

The service iterates `/integration-tests/*`, running each binary with
`--nocapture`; the loop fails fast, so the first failing test masks later ones.
`restart = "no"` keeps the service one-shot. The same `SYS_ADMIN` +
`seccomp=unconfined` grants as dev/prod apply (see below).

On the host the suites still run as ordinary tests, gated by runtime
landlock/userns skip guards:

```sh
cargo test --workspace --all-targets --all-features
```

## Run-time privileges (all modes)

The daemon enables the `landlock` and `seccomp` sandbox features for agent
shells. Its shell backend sets up an isolated mount namespace (user namespace +
bind/tmpfs mounts) before applying Landlock and seccomp filters, **fail-closed**:
if any step is blocked, the spawned shell aborts.

`arion-compose.nix` already grants what this needs:

- `service.capabilities.SYS_ADMIN = true` (→ `cap_add: [SYS_ADMIN]`)
- `out.service.security_opt = [ "seccomp=unconfined" ]`

So you do not pass any `--security-opt` / `--cap-add` by hand.

## Volumes and workspaces

In dev/prod, daemon data and the agent workspace are **docker named volumes**
by default — no host directories are created and the project tree stays clean.
Shared skills live inside the `data` volume's `skills/` subdir. Test mode mounts
none (its scratch tree is an ephemeral `/testdata` tmpfs).

- `data` named volume → `/var/lib/kallip` — agent state, logs, skills (persistent; survives `arion down`, removed by `arion down -v`).
- `workspace` named volume → `/workspace` — the agent workspace root.

Data and workspace can be bind-mounted to a host path via their env vars, when
you want the files on the host (e.g. inspect/persist daemon state, or have the
agent work on a checkout). Shared skills can be overlaid on the data volume's
`skills/` subdir the same way (agent-local skills under
`/var/lib/kallip/agents/<id>/skills/` are unaffected):

```sh
KALLIP_ARION_DATA_PATH=$PWD/data arion up -d        # /var/lib/kallip ← host ./data
KALLIP_ARION_WORKSPACE_PATH=$PWD/ws arion up -d     # /workspace ← host ./ws
KALLIP_ARION_SKILLS_PATH=$PWD/skills arion up -d    # /var/lib/kallip/skills ← host ./skills
```

Don't point `KALLIP_ARION_SKILLS_PATH` at the same host path as
`KALLIP_ARION_DATA_PATH` — the skills subdir would shadow itself
confusingly. The override value must be an absolute, colon-free path (the
compose throws at eval otherwise).

`workspace_root` passed to the daemon API is always resolved as an in-container
path (default `/workspace`); a host bind does not change what the daemon sees.

Each agent needs a `workspace_root` that exists in the container and is
**disjoint** from `/var/lib/kallip`. Pass `workspace_root: /workspace` when
creating an agent via the [daemon API](daemon-api.md); the daemon rejects a
workspace that contains or is contained by the data dir.

## Environment

The compose sets only the daemon defaults. Provider credentials and the operator
token come from `.env`:

| Variable                | Required    | Notes                                                                          |
| ----------------------- | ----------- | ------------------------------------------------------------------------------ |
| `KALLIP_LLM_PROVIDER`   | **yes**     | See [env.md](env.md).                                                          |
| `KALLIP_LLM_MODEL`      | **yes**     | See [env.md](env.md).                                                          |
| `KALLIP_LLM_*_API_KEY`  | conditional | Provider key, e.g. `KALLIP_LLM_DEEPSEEK_API_KEY`.                              |
| `KALLIP_OPERATOR_TOKEN` | no          | If unset, a random `sk-operator-...` token is generated and printed to stdout. |

The compose already sets `KALLIP_DAEMON_ADDR=0.0.0.0:3000` (in dev and prod),
`HOME`, `PATH`, `RUST_LOG`, and `KALLIP_WORKSPACE_ROOT=/workspace` (the
default workspace for clients like the TUI that create an agent without an
explicit `workspace_root`). Do not override `KALLIP_ADVERTISE_URL`; its
default `http://127.0.0.1:3000` is correct because the daemon and agent shells
share the container's network namespace.

## Without Arion (plain Docker)

If you cannot use Arion, build and load the image directly:

```sh
nix build .#kallip-image
docker load < result
docker run --rm \
  --security-opt seccomp=unconfined --cap-add SYS_ADMIN \
  -p 3000:3000 \
  -v kallip_data:/var/lib/kallip \
  -v kallip_workspace:/workspace \
  -e KALLIP_LLM_PROVIDER=deepseek \
  -e KALLIP_LLM_MODEL=deepseek-v4-flash \
  -e KALLIP_LLM_DEEPSEEK_API_KEY="$DEEPSEEK_KEY" \
  kallip:latest
```

Then create an agent via the [daemon API](daemon-api.md) with
`workspace_root: /workspace`.
