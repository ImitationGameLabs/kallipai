# Container images

The kallip services ship as **scratch-based** container images built with
`nixpkgs.dockerTools` — no Dockerfiles. Each image embeds the nix store closure
of its binaries (glibc, the CA bundle, and every runtime dep come from the
closure). Only `x86_64-linux` images are published. There are two
purpose-built images for the split production deploy:

- `packages.kallip-agora-image` — the agora control-plane server. Minimal:
  just the `kallip-agora` binary + the CA bundle (agora is a pure HTTP/Postgres
  server with no shell-out deps).
- `packages.kallip-tagma-image` — the host/"tagma" side: the `kallip-tagma`
  binary (agent host + in-process relay connector) + the `kallip` CLI (whose
  `lesche send` subcommand the agent invokes to address the user) plus the tagma's
  shell toolset (the agent landlock sandbox shells out to
  bash/coreutils/ripgrep/git/pgrep/kill). Carries no tagma-specific baked env —
  the compose `tagma` service sets its own command + env.

The recommended way to run them is [Arion](https://docs.hercules-ci.com/arion/)
(a Nix-native docker-compose). Local dev and the integration-test suite live in
`arion-compose.nix` at the repo root; dev is the default (just `arion up`), and
the integration suite is the single opt-in via `KALLIP_ARION_MODE=test`. Any
other value — including a stale `prod-tagma` / `prod-agora` — is a **hard
error** that points at the standalone prod files, not a silent fallback:

| Mode             | Command                              | Services                  | Image source                                    |
| ---------------- | ------------------------------------ | ------------------------- | ----------------------------------------------- |
| **dev**          | `arion up -d` (default)              | agora + postgres          | `packages.default`, run via `useHostStore`      |
| **dev** +`tagma` | `COMPOSE_PROFILES=tagma arion up -d` | agora + postgres + tagma  | `packages.default`, run via `useHostStore`      |
| **test**         | `KALLIP_ARION_MODE=test arion up`    | tagma (integration suite) | `packages.kallip-integration-tests`, host store |

Production is split into two **standalone compositions** under
`nix/prod-composes/`, each a flat single-mode file invoked with `arion -f`
(run from the repo root so `.env` resolves):

| Composition | Command                                      | Services         | Image source                                    |
| ----------- | -------------------------------------------- | ---------------- | ----------------------------------------------- |
| **tagma**   | `arion -f nix/prod-composes/tagma.nix up -d` | tagma            | `packages.kallip-tagma-image` (pre-built)       |
| **agora**   | `arion -f nix/prod-composes/agora.nix up -d` | agora + postgres | `packages.kallip-agora-image` + `postgres:17.5` |

The two prod halves run on **separate hosts** (the tagma host and the agora
server) and carry distinct compose project names (`kallipai-tagma` /
`kallipai-agora`) so their containers/volumes are unambiguous in
`docker ps` / `docker volume ls`. The tagma's in-process relay connector reaches
the agora over its public HTTPS URL.

`dev` is a **two-phase** flow (the tagma's relay connector cannot enroll until a
user signs up and mints a code); see [development.md](../development.md) for the
bring-up commands and flow.

## Prerequisites

Arion and a Docker (or Podman with docker socket) daemon. On NixOS:

```nix
environment.systemPackages = [ pkgs.arion ];
virtualisation.docker.enable = true;   # or podman + dockerSocket
```

Copy `.env.example` to `.env` and fill in the LLM provider credentials. Arion
reads `.env` via `service.env_file`.

## Dev: `arion up` (default)

The bring-up flow (two-phase, because the relay connector needs an enrollment
code) and the iteration loop are documented in
[development.md](../development.md). This section covers the dev-only mechanics.

Dev skips the image bake for the kallip services. `useHostStore` bind-mounts the
host `/nix/store` read-only into the tagma/agora containers, so they run
straight out of the crane workspace (`packages.default`) and a rebuild is picked
up without an in-compose bake; postgres uses the official `postgres:17.5` image.

The tagma is gated behind the `tagma` profile, so a plain `arion up` brings up
only the agora + lesche side. The tagma's relay connector enrolls on first boot
and needs a code that cannot exist until a user signs up — with
`KALLIP_TAGMA_RELAY_AGORA_URL` set but no code it degrades to local-only (logs an
error, keeps serving local agents; see [Relay bootstrap](#relay-bootstrap)).

Dev uses a **per-service subdomain** topology with no edge proxy. Browsers
resolve `*.localhost` to `127.0.0.1` natively, so the web app (`deno task dev`
on the host at `:5173`) reaches the agora at `http://agora.localhost:7100` and
the lesche at `http://lesche.localhost:7200` directly (each service publishes
its own port). The session cookie carries `Domain=localhost`
(`KALLIP_AGORA_SESSION_COOKIE_DOMAIN`), so the cookie set at login on the agora
is sent to the lesche too — both subdomains share the registrable domain
`localhost` (same-site under `SameSite=Strict`), and CORS on each service
allows the `http://localhost:5173` origin with credentials. Dev is validated on
Chrome/Firefox (Safari has historical `*.localhost`/`Domain=localhost` quirks).
The tagma container reaches the two services via compose DNS
(`http://agora:7100`, `http://lesche:7200`), not `*.localhost`.

## Production

Production is split into two standalone compositions under `nix/prod-composes/`.
Each is a flat, single-mode file; invoke it from the **repo root** (so `.env`
resolves):

### tagma — `arion -f nix/prod-composes/tagma.nix up -d`

Brings up the tagma (agent host + in-process relay connector) from
`packages.kallip-tagma-image`. The relay connector talks to the prod-deployed
services over the public internet: the agora subdomain
(`KALLIP_TAGMA_RELAY_AGORA_URL`, e.g. `https://agora.kallipai.com`) for
enrollment only — the stored tagma token is reused thereafter — and the lesche
subdomain (`KALLIP_TAGMA_RELAY_LESCHE_URL`, e.g. `https://lesche.kallipai.com`)
for its tunnel,
envelope POSTs, and key-exchange responses (the per-service subdomain
topology).

> **Note**: the data-plane relay (`kallip-lesche`) is a separate service from
> the agora, reached over its `/internal/*` ControlPlane API guarded by a shared
> secret (`KALLIP_AGORA_INTERNAL_TOKEN` on the agora, `KALLIP_LESCHE_AGORA_TOKEN`
> on the lesche). The operator's edge HOST-routes the two subdomains to the two
> services and the session cookie carries `Domain=<parent>`
> (`KALLIP_AGORA_SESSION_COOKIE_DOMAIN`) so login on `agora.<d>` is recognized on
> `lesche.<d>`. `/internal` is reached by the lesche over the private network,
> never via the public edge.

```sh
arion -f nix/prod-composes/tagma.nix up -d
arion -f nix/prod-composes/tagma.nix logs -f
```

Secure the tagma's published `3000` port (the operator API) — do not expose it
on a public host without a firewall / TLS reverse proxy in front.

### agora — `arion -f nix/prod-composes/agora.nix up -d`

Brings up the agora (from `packages.kallip-agora-image`) + lesche (from
`packages.kallip-lesche-image`) + postgres (official `postgres:17.5` image) —
the three server-side services co-located on one host. **Neither the agora nor
the lesche is published** — both sit behind the operator's TLS-terminating edge
proxy, which HOST-routes `agora.<d>` → `agora:7100` and `lesche.<d>` →
`lesche:7200` (per-service subdomains) and sets `X-Forwarded-For`; configure
`KALLIP_AGORA_TRUSTED_PROXIES` to the proxy's CIDR. All agora/lesche env (DB
url, WebAuthn RP, CORS, cookie domain, admin token, the internal shared secret)
and the postgres credentials come from `.env`.

```sh
arion -f nix/prod-composes/agora.nix up -d
arion -f nix/prod-composes/agora.nix logs -f
```

## Relay bootstrap

Applies to both dev and the prod-tagma composition (the only compositions that
run the tagma with a relay configured). The tagma's relay connector enrolls on
its **first** boot using `KALLIP_TAGMA_RELAY_ENROLLMENT_CODE` (a single-use
`sk-enroll-...` minted via the agora dashboard after a user signs up). After
that it persists the tagma token under `KALLIP_DATA_DIR/credentials/` (i.e. inside the
`data` volume) and reuses it. Leave the code unset on subsequent boots. The
first-boot `enroll()` is not retried in code: on a missing/unreachable agora it
logs an error, leaves the relay unset, and keeps serving local agents (the
lesche message route returns 503). The tagma service is
`restart: unless-stopped`, so it comes
back once the code is supplied / the agora is reachable (check
`arion logs tagma`).

## Integration tests: `KALLIP_ARION_MODE=test arion up`

Test mode runs the workspace's integration tests (`[[test]]` targets) **inside
the container** to confirm the sandbox and shell backends behave correctly in
the containerized environment the tagma ships in. Today that covers the
`sandbox` suite (`crates/kallip-tagma/tests/sandbox/` — a scripted
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
arion logs tagma
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

## Run-time privileges (tagma modes)

The tagma enables the `landlock` and `seccomp` sandbox features for agent
shells. Its shell backend sets up an isolated mount namespace (user namespace +
bind/tmpfs mounts) before applying Landlock and seccomp filters, **fail-closed**:
if any step is blocked, the spawned shell aborts.

`arion-compose.nix` (and `nix/prod-composes/tagma.nix`) already grant what this
needs — on the `tagma` service only, in every composition that runs the tagma
(dev / test / the prod-tagma composition):

- `service.capabilities.SYS_ADMIN = true` (→ `cap_add: [SYS_ADMIN]`)
- `out.service.security_opt = [ "seccomp=unconfined" ]`

The agora and postgres services need no special privileges.

## Volumes and workspaces

In dev and the prod-tagma composition, tagma data and the agent workspace are
**docker named volumes** — no host directories are created and the project tree
stays clean. Shared skills live inside the `data` volume's `skills/` subdir, and
the tagma credentials (device key + tagma token) live under
`/var/lib/kallip/credentials/` inside the `data` volume. The agora + postgres
services add their own volumes in the compositions that run them. Test mode
mounts none (its scratch tree is an ephemeral `/testdata` tmpfs).

- `data` named volume → `/var/lib/kallip` — agent state, logs, skills, and the tagma credentials (persistent; survives `arion down`, removed by `arion down -v`).
- `workspace` named volume → `/workspace` — the agent workspace root.
- `pgdata` named volume → `/var/lib/postgresql/data` — the agora's Postgres store (dev + the prod-agora composition).

**In dev only**, data and workspace can be bind-mounted to a host path via
their env vars, when you want the files on the host (e.g. inspect/persist tagma
state, or have the agent work on a checkout). Shared skills can be overlaid on
the data volume's `skills/` subdir the same way. Prod-tagma uses plain
named volumes — if you need tagma state on a specific disk, pin it at the
docker layer (data-root) or edit the compose:

```sh
KALLIP_ARION_DATA_PATH=$PWD/data arion up -d        # /var/lib/kallip ← host ./data
KALLIP_ARION_WORKSPACE_PATH=$PWD/ws arion up -d     # /workspace ← host ./ws
KALLIP_ARION_SKILLS_PATH=$PWD/skills arion up -d    # /var/lib/kallip/skills ← host ./skills
```

Don't point `KALLIP_ARION_SKILLS_PATH` at the same host path as
`KALLIP_ARION_DATA_PATH` — the skills subdir would shadow itself
confusingly. The override value must be an absolute, colon-free path (the
compose throws at eval otherwise).

`workspace_root` passed to the tagma API is always resolved as an in-container
path (default `/workspace`); a host bind does not change what the tagma sees.

Each agent needs a `workspace_root` that exists in the container and is
**disjoint** from `/var/lib/kallip`. Pass `workspace_root: /workspace` when
creating an agent via the [tagma API](tagma-api.md); the tagma rejects a
workspace that contains or is contained by the data dir.

## Environment

The compose sets the per-service defaults (the tagma's `KALLIP_TAGMA_ADDR`,
`HOME`, `PATH`, `RUST_LOG`, `KALLIP_WORKSPACE_ROOT`; the dev agora's WebAuthn
RP/CORS/cookie values). Provider credentials, tokens, and the prod-agora deploy
secrets come from `.env` (compose precedence: `service.environment` wins over
`env_file`, so anything the compose hardcodes for dev is NOT overridable via
`.env` in that mode — prod reads everything from `.env` instead).

Tagma (dev / the prod-tagma composition):

| Variable                | Required    | Notes                                                                          |
| ----------------------- | ----------- | ------------------------------------------------------------------------------ |
| `KALLIP_LLM_PROVIDER`   | **yes**     | See [env.md](env.md).                                                          |
| `KALLIP_LLM_MODEL`      | **yes**     | See [env.md](env.md).                                                          |
| `KALLIP_LLM_*_API_KEY`  | conditional | Provider key, e.g. `KALLIP_LLM_DEEPSEEK_API_KEY`.                              |
| `KALLIP_OPERATOR_TOKEN` | no          | If unset, a random `sk-operator-...` token is generated and printed to stdout. |

Relay connector (dev / the prod-tagma composition) — activate + enroll via `.env`:

| Variable                             | Required             | Notes                                                                                                                                                        |
| ------------------------------------ | -------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `KALLIP_TAGMA_RELAY_AGORA_URL`       | **yes** (prod-tagma) | The prod-agora deploy's public HTTPS URL, for enrollment only. Setting any value activates the relay. (Dev hardcodes `http://agora:7100`.)                   |
| `KALLIP_TAGMA_RELAY_LESCHE_URL`      | no                   | The prod-lesche deploy's public HTTPS URL (tunnel + envelopes + KEX). Defaults to the `KALLIP_TAGMA_RELAY_AGORA_URL` origin; set it for the subdomain split. |
| `KALLIP_TAGMA_RELAY_ENROLLMENT_CODE` | first boot only      | A `sk-enroll-...` minted via the agora dashboard. Remove after the first successful enroll.                                                                  |

Agora + postgres (the prod-agora composition) — `.env` only (dev hardcodes
localhost values):

| Variable                          | Required                      | Notes                                                                                                                                  |
| --------------------------------- | ----------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `KALLIP_AGORA_DATABASE_URL`       | **yes** (prod-agora)          | `postgres://<USER>:<POSTGRES_PASSWORD>@postgres:5432/<DB>` — user/db/password must match the `POSTGRES_*` vars.                        |
| `POSTGRES_USER`                   | **yes** (prod-agora)          | The postgres superuser role; must match the user in `KALLIP_AGORA_DATABASE_URL` (the image defaults to `postgres`).                    |
| `POSTGRES_PASSWORD`               | **yes** (prod-agora)          | The postgres superuser password (read by the postgres image).                                                                          |
| `POSTGRES_DB`                     | **yes** (prod-agora)          | The initial db; must match the db in `KALLIP_AGORA_DATABASE_URL` (the image defaults to `postgres`).                                   |
| `KALLIP_AGORA_WEBAUTHN_RP_ID`     | **yes** (prod-agora)          | The registrable domain passkeys bind to; cannot change without invalidating every passkey.                                             |
| `KALLIP_AGORA_WEBAUTHN_RP_ORIGIN` | **yes** (prod-agora)          | The exact origin the web app is served from (`https://app.example.com`).                                                               |
| `KALLIP_AGORA_CORS_ORIGINS`       | **yes** (prod-agora)          | The app origin(s); never a wildcard on a public deploy.                                                                                |
| `KALLIP_AGORA_COOKIE_SECURE`      | no (defaults true)            | Keep `true` behind TLS; `false` only for local HTTP dev (dev hardcodes `false`).                                                       |
| `KALLIP_AGORA_TRUSTED_PROXIES`    | **yes** behind a remote proxy | Loopback-only by default and **cleared on a public bind**; set to the proxy's CIDR so X-Forwarded-For / per-client rate limiting work. |
| `KALLIP_AGORA_ADMIN_TOKEN`        | no                            | Stable admin token; else generated per boot and printed to `arion logs agora`.                                                         |

Do not override `KALLIP_ADVERTISE_URL`; its default `http://127.0.0.1:3000` is
correct because the tagma and agent shells share the container's network
namespace.

## Without Arion (plain Docker)

If you cannot use Arion, build and load the image(s) directly. The tagma runs
from `kallip-tagma-image`:

```sh
nix build .#kallip-tagma-image
docker load < result
docker run --rm \
  --security-opt seccomp=unconfined --cap-add SYS_ADMIN \
  -p 3000:3000 \
  -v kallipai-tagma_data:/var/lib/kallip \
  -v kallipai-tagma_workspace:/workspace \
  -e KALLIP_LLM_PROVIDER=deepseek \
  -e KALLIP_LLM_MODEL=deepseek-v4-flash \
  -e KALLIP_LLM_DEEPSEEK_API_KEY="$DEEPSEEK_KEY" \
  kallip-tagma:latest kallip-tagma
```

(The `kallip-tagma-image` has no default `Cmd` — pass the binary name
`kallip-tagma` explicitly.)

The agora runs from `kallip-agora-image` (behind your own TLS reverse proxy +
a postgres):

```sh
nix build .#kallip-agora-image
docker load < result
docker run --rm \
  -e KALLIP_AGORA_DATABASE_URL=postgres://kallip:...@postgres:5432/kallip \
  -e KALLIP_AGORA_WEBAUTHN_RP_ID=agora.example.com \
  -e KALLIP_AGORA_WEBAUTHN_RP_ORIGIN=https://app.example.com \
  -e KALLIP_AGORA_CORS_ORIGINS=https://app.example.com \
  kallip-agora:latest
```

Then create an agent via the [tagma API](tagma-api.md) with
`workspace_root: /workspace`.
