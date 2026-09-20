---
title: Container images
description: Published container images and how to run them.
order: 10
internal: true
---

The platform's services ship as **scratch-based** container images built with
`nixpkgs.dockerTools`: no Dockerfiles; each image embeds the nix store
closure of its binaries. Only `x86_64-linux` images are published. Two
purpose-built images cover the split production deploy:

- `packages.kallip-tagma-image`: the tagma service (the agent host with its
  relay connector) plus the `kallip` CLI and the tagma's shell toolset
  (bash, coreutils, ripgrep, git, pgrep, kill). No baked-in configuration,
  the compose `tagma` service sets its own command and env.
- `packages.kallip-archeion-image`: the platform's control-plane service:
  the service binary and the CA bundle, nothing else.

The recommended way to run them is [Arion](https://docs.hercules-ci.com/arion/),
a Nix-native docker-compose. Each composition is a flat, single-purpose file
under `compose/` (`compose/dev/`, `compose/prod/`); the repo-root
`arion-compose.nix` re-exports `compose/dev/polis.nix`, so a plain `arion up`
brings up the dev platform side. The others are invoked with `arion -f`:

| Composition    | Command                                | Services                                                        | Image source                               |
| -------------- | -------------------------------------- | --------------------------------------------------------------- | ------------------------------------------ |
| **dev**        | `arion up -d` (default)                | caddy + platform services + their postgres stores               | `packages.default`, run via `useHostStore` |
| **dev** tagma  | `arion -f compose/dev/tagma.nix up -d` | tagma                                                           | `packages.default`, run via `useHostStore` |
| **test**       | `arion -f compose/dev/test.nix up`     | tagma (integration suite)                                       | `packages.kallip-integration-tests`        |

Production is split into two **standalone compositions** under `compose/prod/`
(run from the repo root so `.env` resolves):

| Composition  | Command                                 | Services                                  | Image source                                  |
| ------------ | --------------------------------------- | ----------------------------------------- | --------------------------------------------- |
| **tagma**    | `arion -f compose/prod/tagma.nix up -d` | tagma                                     | `packages.kallip-tagma-image` (pre-built)     |
| **platform** | `arion -f compose/prod/polis.nix up -d` | platform services + their postgres stores | `packages.kallip-archeion-image` + `postgres:17.5` + siblings |

The two production halves run on **separate hosts** with distinct compose
project names (`kallipai-tagma` / `kallipai-platform`) so their containers
and volumes are unambiguous in `docker ps` / `docker volume ls`. The
platform services can alternatively deploy on the NixOS host through
`services.kallipai.polis` (see the NixOS module section below).

## Prerequisites

Arion and a Docker (or Podman with docker socket) daemon. On NixOS:

```nix
environment.systemPackages = [ pkgs.arion ];
virtualisation.docker.enable = true;   # or podman + dockerSocket
```

Copy `.env.example` to `.env` and fill in the LLM provider credentials
(Arion reads `.env` via `service.env_file`).

### Development Compositions

The dev bring-up is a two-phase flow (the relay connector cannot enroll until
a user has signed up and minted an enrollment code); the commands and the
iteration loop are documented in the
[development setup guide](../development/setup.md). This section covers the
dev-only mechanics: `useHostStore` bind-mounts the host `/nix/store`
read-only into the containers, so they run straight out of the workspace
build (`packages.default`) and a rebuild needs no in-compose bake; postgres
uses the official `postgres:17.5` image.

The dev tagma lives in its own composition (`compose/dev/tagma.nix`), so a
plain `arion up` brings up only the platform side; bring the tagma up with
`arion -f compose/dev/tagma.nix up -d`. With
`KALLIP_TAGMA_RELAY_ENROLLMENT_CODE` unset it degrades to local-only (it logs
an error and keeps serving local agents; see
[Relay bootstrap](#relay-bootstrap)).

Dev is fronted by a Caddy edge proxy that terminates TLS for
`*.kallipai.lan` with an mkcert certificate, so the stack is reachable
cross-machine on the LAN and the browser sees a secure context. The dev
domain comes from `.env` (`.env.example` sets `KALLIP_DOMAIN=kallipai.lan`).
One-time host setup is covered in the
[development setup guide](../development/setup.md).

### Production

#### The Tagma Host: `arion -f compose/prod/tagma.nix up -d`

Brings up the tagma service from `packages.kallip-tagma-image`. The relay
connector talks to the deployed platform over the public internet through one
origin (`KALLIP_POLIS_URL`, e.g. `https://api.kallipai.com`): enrollment
happens once on first boot against that origin, and the relay connection is
held through it afterwards.

```sh
arion -f compose/prod/tagma.nix up -d
arion -f compose/prod/tagma.nix logs -f
```

Secure the tagma's published `3000` port (the operator API): do not expose
it on a public host without a firewall / TLS reverse proxy in front.

#### The Platform Host: `arion -f compose/prod/polis.nix up -d`

Brings up the platform's services (identity control plane, relay data plane,
files service, instances proxy) plus one postgres store per stateful service
(`postgres:17.5`), co-located on one host. **None of the services is
published**: all sit behind your TLS-terminating edge proxy, which
path-routes the single `api.<your-domain>` host by service and sets
`X-Forwarded-For`; configure `KALLIP_ARCHEION_TRUSTED_PROXIES` to the
proxy's CIDR. Secret-bearing env and the postgres credentials come from
`.env`; operational env is pinned in `service.environment`, which overrides
`env_file`.

The edge route table (Caddy; dev's `compose/dev/Caddyfile.dev` mirrors it on the
loopback face; unlisted `/v1/*` prefixes are a real 404):

```caddy
api.<your-domain> {
	handle_path /v1/archeion/*  { reverse_proxy archeion:7100 }
	handle_path /v1/lesche/*    { reverse_proxy lesche:7200 } # flush_interval -1 for SSE
	@files path /v1/files /v1/files/*
	handle @files               { uri strip_prefix /v1/files    reverse_proxy files:7400 }
	handle_path /v1/instances/* { reverse_proxy instances:7300 }
}
```

```sh
arion -f compose/prod/polis.nix up -d
arion -f compose/prod/polis.nix logs -f
```

#### The Platform Services: the NixOS Module

For a step-by-step walkthrough of a full host deployment (module import,
token file, and verification), see the
[NixOS deployment guide](nixos/index.md); this section is the option-level
reference.

The platform services run as systemd units on the NixOS host, enabled with
one switch:

```nix
services.kallipai.polis = {
  enable = true;
  archeionPackage = inputs.self.packages.x86_64-linux.kallip-archeion;
  leschePackage = inputs.self.packages.x86_64-linux.kallip-lesche;
  filesPackage = inputs.self.packages.x86_64-linux.kallip-files;
  instancesPackage = inputs.self.packages.x86_64-linux.kallip-instances;
};
```

The module binds all services to localhost (the host's reverse proxy is the
only ingress), stands up a shared PostgreSQL for the stateful services
(per-service databases, unix-socket peer auth), and orders the units so the
control plane starts before its consumers. Tuning options are nullable and
default to the binaries' own defaults; see `nix/nixos-modules.nix`.

Routing the services' public names is the deployment's own edge
configuration; see the reverse proxy chapter in the
[NixOS deployment guide](nixos/index.md) for a copy-paste `services.caddy`
example (the relay data-plane route should flush immediately so the event
stream never buffers behind the proxy).

#### The Web Site Root

On NixOS, `config.services.kallipai.web.distWithRuntimeConfig` is the site
root your edge serves with a plain `file_server` block: the web bundle
as-is while `services.kallipai.web.runtimeConfig` is empty, or with a
runtime config payload baked into `config.js` once keys are set:

```nix
services.kallipai.web = {
  enable = true;
  runtimeConfig = {
    domain = "kallipai.lan";
  };
};
```

Unset keys fall back to the app-side derivation; the bundle itself is
deployment-independent. The dev form of the same site is the host vite
server behind the dev Caddyfile.

### Relay Bootstrap

The tagma enrolls with the platform on its **first** boot, using a
single-use enrollment code minted via the platform dashboard after a user
signs up (`KALLIP_TAGMA_RELAY_ENROLLMENT_CODE`). The issued tagma token is
persisted inside the `data` volume and reused after that; leave the code
unset on subsequent boots. If enrollment cannot complete, the tagma logs an
error, keeps serving local agents, and comes back to it on a later boot
(`restart: unless-stopped`): check `arion logs tagma`.

### Run-Time Privileges

The tagma runs agent shells inside a sandbox (kernel-level filtering plus an
isolated mount namespace), applied **fail-closed**: if any step is blocked,
the spawned shell aborts. The three compositions that run the tagma:
`compose/dev/tagma.nix`, `compose/dev/test.nix`, and
`compose/prod/tagma.nix`: grant what this needs, on the `tagma` service
only:

- `service.capabilities.SYS_ADMIN = true` (→ `cap_add: [SYS_ADMIN]`)
- `out.service.security_opt = [ "seccomp=unconfined" ]`

The platform services and both postgres services need no special
privileges.

Docker's default shape is rootful: the container's uid 0 is the host's uid
0, exactly what the sandbox guard refuses, since every spawned instance
would be a host-root process. Prefer a rootless or userns-remapped runtime.
Where that is not an option, set
`KALLIP_TAGMA_ACCEPT_UNSAFE_RUN_AS_ROOT=1` (exact value) in the tagma
service's environment and accept the per-boot warning; anything else:
unset, empty, `0`, keeps the refusal.

The grant lives on the compose `service.environment` line, not in `.env`:
the tagma services also load `.env` (via `env_file`), but an explicit
`service.environment` entry wins, so the consent is granted and revoked by
editing the compose file, never the env file.

### Volumes and Workspaces

In dev and the prod-tagma composition, tagma data and the agent workspace
are **docker named volumes**: no host directories are created and the
project tree stays clean. Shared skills live inside the `kallipai_tagma_data`
volume's `skills/` subdir. The platform host and each postgres service add
their own volumes in the compositions that run them. The test composition
mounts none (its scratch tree is an ephemeral `/testdata` tmpfs).

- `kallipai_tagma_data` named volume → `/var/lib/kallipai/tagmata/main`: agent state, logs, skills, and the tagma's credentials (persistent; survives `arion down`, removed by `arion down -v`).
- `workspace` named volume → `/workspace`: the agent workspace root.
- `archeion_pgdata` named volume → `/var/lib/postgresql/data`: the identity control plane's Postgres store (dev + the prod-platform composition).
- `lesche_pgdata` named volume → `/var/lib/postgresql/data`: the relay data plane's Postgres chat store (dev + the prod-platform composition).
- `files_pgdata` named volume → `/var/lib/postgresql/data`: the files service's Postgres store (dev + the prod-platform composition).
- `kallipai_files_blobs` named volume → `/var/lib/kallipai/files/blobs`: the files service's content-addressed blob store (dev + the prod-platform composition).

**In dev only**, data and workspace can be bind-mounted to a host path via
their env vars (inspect/persist tagma state, or have the agent work on a
checkout); shared skills overlay the same way. Prod-tagma uses plain named
volumes: to pin tagma state on a specific disk, edit the compose:

```sh
KALLIP_ARION_DATA_PATH=$PWD/data arion up -d        # /var/lib/kallipai/tagmata/main ← host ./data
KALLIP_ARION_WORKSPACE_PATH=$PWD/ws arion up -d     # /workspace ← host ./ws
KALLIP_ARION_SKILLS_PATH=$PWD/skills arion up -d    # /var/lib/kallipai/tagmata/main/skills ← host ./skills
```

Don't point `KALLIP_ARION_SKILLS_PATH` at the same host path as
`KALLIP_ARION_DATA_PATH`; the skills subdir would shadow itself
confusingly. The override value must be an absolute, colon-free path (the
compose throws at eval otherwise).

`workspace_root` passed to the tagma API is always resolved as an
in-container path (default `/workspace`); a host bind does not change what
the tagma sees.

Each agent needs a `workspace_root` that exists in the container and is
**disjoint** from `/var/lib/kallipai/tagmata/main`. Pass
`workspace_root: /workspace` when creating an agent via the tagma API; the
tagma rejects a workspace that contains or is contained by the data dir.

### Environment

The compose files hardcode per-service defaults, and `.env` supplies
the rest: provider credentials, tokens, and the prod-platform deploy
secrets. Compose precedence means `service.environment` wins over
`env_file`, so values the dev compose hardcodes are not overridable
via `.env` in that mode. The full variable tables live in the
[environment-variables reference](../reference/environment-variables/index.md).

### Without Arion (Plain Docker)

If you cannot use Arion, build and load the image(s) directly. The tagma
runs from `kallip-tagma-image`:

```sh
nix build .#kallip-tagma-image
docker load < result
# The tag is the built git version, not "latest" — take it from the
# "Loaded image" line docker load just printed:
docker images | grep kallip-tagma
docker run --rm \
  --security-opt seccomp=unconfined --cap-add SYS_ADMIN \
  -p 3000:3000 \
  -v kallipai-tagma_data:/var/lib/kallipai/tagmata/main \
  -v kallipai-tagma_workspace:/workspace \
  -e KALLIP_LLM_PROVIDER=deepseek \
  -e KALLIP_LLM_MODEL=deepseek-v4-flash \
  -e KALLIP_LLM_DEEPSEEK_API_KEY="$DEEPSEEK_KEY" \
  kallip-tagma:<gitVersion> kallip-tagma
```

(The image has no default `Cmd`: pass the binary name `kallip-tagma`
explicitly.)

The platform's control plane runs from `kallip-archeion-image` (behind your
own TLS reverse proxy and a postgres):

```sh
nix build .#kallip-archeion-image
docker load < result
# The tag is the built git version, not "latest" — take it from the
# "Loaded image" line docker load just printed:
docker images | grep kallip-archeion
docker run --rm \
  -e KALLIP_ARCHEION_DATABASE_URL=postgres://kallip:...@postgres:5432/kallip \
  -e KALLIP_ARCHEION_WEBAUTHN_RP_ID=app.<your-domain> \
  -e KALLIP_ARCHEION_WEBAUTHN_RP_ORIGIN=https://app.<your-domain> \
  -e KALLIP_ARCHEION_CORS_ORIGINS=https://app.<your-domain> \
  kallip-archeion:<gitVersion>
```

Then create an agent via the tagma API with `workspace_root: /workspace`.
