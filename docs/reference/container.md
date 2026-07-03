# Docker image

`just-agent-daemon` ships as a container image built with
`nixpkgs.dockerTools` — no Dockerfile. It shares the same crane-built workspace
as the `just-agent-tarball` package, so the binaries inside are bit-identical to
a `nix build .#just-agent-tarball` build. The image is **scratch-based**: glibc,
bash, the CA bundle, and every other runtime dependency come from the nix store
closure embedded in the image. Only `x86_64-linux` is published.

The recommended way to run it is [Arion](https://docs.hercules-ci.com/arion/) (a
Nix-native docker-compose), configured by `arion-compose.nix` at the repo root.
Two modes are switched by an env var:

| Mode     | Command                               | Image source                             | Host nix store |
| -------- | ------------------------------------- | ---------------------------------------- | -------------- |
| **dev**  | `arion up -d` (default)               | `packages.default`, run via useHostStore | shared (ro)    |
| **prod** | `JUST_AGENT_ARION_MODE=prod arion up` | `packages.just-agent-image` (pre-built)  | not shared     |

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
mkdir -p data ws
# after editing Rust source:
nix build .#default        # rebuilds the workspace (incremental via crane)
arion up -d                # picks up the new binary
```

Arion resolves the same workspace path as `nix build .#default`, so there is no
store duplication.

## Production: `JUST_AGENT_ARION_MODE=prod arion up`

```sh
mkdir -p data ws
JUST_AGENT_ARION_MODE=prod arion up -d
arion logs -f
```

Arion builds `packages.just-agent-image` (the flake's two-layer `buildImage`),
loads it, and runs it — one command, no manual `docker load`.

## Run-time privileges (both modes)

The daemon enables the `landlock` and `seccomp` sandbox features for agent
shells. Its shell backend sets up an isolated mount namespace (user namespace +
bind/tmpfs mounts) before applying Landlock and seccomp filters, **fail-closed**:
if any step is blocked, the spawned shell aborts.

`arion-compose.nix` already grants what this needs:

- `service.capabilities.SYS_ADMIN = true` (→ `cap_add: [SYS_ADMIN]`)
- `out.service.security_opt = [ "seccomp=unconfined" ]`

So you do not pass any `--security-opt` / `--cap-add` by hand.

## Volumes and workspaces

`arion-compose.nix` bind-mounts two paths (create them first):

- `./data` → `/var/lib/just-agent` — agent state, logs, skills (persistent).
- `./ws` → `/workspace` — the agent workspace root.

Each agent needs a `workspace_root` that exists in the container and is
**disjoint** from `/var/lib/just-agent`. Pass `workspace_root: /workspace` when
creating an agent via the [daemon API](daemon-api.md); the daemon rejects a
workspace that contains or is contained by the data dir.

## Environment

The compose sets only the daemon defaults. Provider credentials and the operator
token come from `.env`:

| Variable                    | Required    | Notes                                                                          |
| --------------------------- | ----------- | ------------------------------------------------------------------------------ |
| `JUST_LLM_PROVIDER`         | **yes**     | See [env.md](env.md).                                                          |
| `JUST_LLM_MODEL`            | **yes**     | See [env.md](env.md).                                                          |
| `JUST_LLM_*_API_KEY`        | conditional | Provider key, e.g. `JUST_LLM_DEEPSEEK_API_KEY`.                                |
| `JUST_AGENT_OPERATOR_TOKEN` | no          | If unset, a random `sk-operator-...` token is generated and printed to stdout. |

The compose already sets `JUST_AGENT_DAEMON_ADDR=0.0.0.0:3000` (in both modes),
`HOME`, `PATH`, and `RUST_LOG`. Do not override `JUST_AGENT_ADVERTISE_URL`; its
default `http://127.0.0.1:3000` is correct because the daemon and agent shells
share the container's network namespace.

## Without Arion (plain Docker)

If you cannot use Arion, build and load the image directly:

```sh
nix build .#just-agent-image
docker load < result
docker run --rm \
  --security-opt seccomp=unconfined --cap-add SYS_ADMIN \
  -p 3000:3000 \
  -v "$PWD/data:/var/lib/just-agent" \
  -v "$PWD/ws:/workspace" \
  -e JUST_LLM_PROVIDER=deepseek \
  -e JUST_LLM_MODEL=deepseek-v4-flash \
  -e JUST_LLM_DEEPSEEK_API_KEY="$DEEPSEEK_KEY" \
  just-agent:latest
```

Then create an agent via the [daemon API](daemon-api.md) with
`workspace_root: /workspace`.
