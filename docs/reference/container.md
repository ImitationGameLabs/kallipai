# Container images

The kallip services ship as **scratch-based** container images built with
`nixpkgs.dockerTools` — no Dockerfiles. Each image embeds the nix store closure
of its binaries (glibc, the CA bundle, and every runtime dep come from the
closure). Only `x86_64-linux` images are published. There are two
purpose-built images for the split production deploy:

- `packages.kallip-archeion-image` — the archeion control-plane server. Minimal:
  just the `kallip-archeion` binary + the CA bundle (archeion is a pure HTTP/Postgres
  server with no shell-out deps).
- `packages.kallip-tagma-image` — the host/"tagma" side: the `kallip-tagma`
  binary (agent host + in-process relay connector) + the `kallip` CLI (whose
  `lesche send` subcommand the agent invokes to address the user) plus the tagma's
  shell toolset (the agent landlock sandbox shells out to
  bash/coreutils/ripgrep/git/pgrep/kill). Carries no tagma-specific baked env —
  the compose `tagma` service sets its own command + env.

The recommended way to run them is [Arion](https://docs.hercules-ci.com/arion/)
(a Nix-native docker-compose). Each composition is a flat, single-purpose file
under `compose/` (dev: `compose/dev/`, prod: `compose/prod/`); the repo-root
`arion-compose.nix` is just a one-line shim that re-exports
`compose/dev/polis.nix`, so arion's auto-discovery still makes a plain
`arion up` bring up the dev archeion side. The others are invoked with `arion -f`:

| Composition   | Command                                      | Services                          | Image source                                    |
| ------------- | -------------------------------------------- | --------------------------------- | ----------------------------------------------- |
| **dev**       | `arion up -d` (default)                      | caddy + archeion + lesche + archeion-postgres + lesche-postgres | `packages.default`, run via `useHostStore`      |
| **dev** tagma | `arion -f compose/dev/tagma.nix up -d`       | tagma                             | `packages.default`, run via `useHostStore`      |
| **test**      | `arion -f compose/dev/test.nix up`           | tagma (integration suite)         | `packages.kallip-integration-tests`, host store |

Production is split into two **standalone compositions** under
`compose/prod/`, each a flat single-mode file invoked with `arion -f`
(run from the repo root so `.env` resolves):

| Composition | Command                                      | Services         | Image source                                    |
| ----------- | -------------------------------------------- | ---------------- | ----------------------------------------------- |
| **tagma**   | `arion -f compose/prod/tagma.nix up -d` | tagma            | `packages.kallip-tagma-image` (pre-built)       |
| **archeion**   | `arion -f compose/prod/polis.nix up -d` | archeion + lesche + archeion-postgres + lesche-postgres | `packages.kallip-archeion-image` + `packages.kallip-lesche-image` + `postgres:17.5` |

The two prod halves run on **separate hosts** (the tagma host and the archeion
server) and carry distinct compose project names (`kallipai-tagma` /
`kallipai-archeion`) so their containers/volumes are unambiguous in
`docker ps` / `docker volume ls`. The tagma's in-process relay connector reaches
the archeion over its public HTTPS URL.
The polis services can alternatively deploy on the NixOS host through `services.kallipai.polis` (see the polis section below).

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
host `/nix/store` read-only into the tagma/archeion/files containers, so they run
straight out of the crane workspace (`packages.default`) and a rebuild is picked
up without an in-compose bake; postgres uses the official `postgres:17.5` image.

The dev tagma lives in a separate composition (`compose/dev/tagma.nix`), so a
plain `arion up` brings up only the archeion side; bring the tagma up with
`arion -f compose/dev/tagma.nix up -d`. It runs on the host network and reaches
the platform edge at `KALLIP_POLIS_URL` (`http://127.0.0.1:7443`, Caddy's loopback plaintext face). Its relay connector enrolls on first
boot and needs a code that cannot exist until a user signs up — with
`KALLIP_TAGMA_RELAY_ENROLLMENT_CODE` unset it degrades to local-only (logs an
error, keeps serving local agents; see [Relay bootstrap](#relay-bootstrap)).

Dev is fronted by a **Caddy edge proxy** (`services.caddy`) that terminates TLS
for `*.<devDomain>` with an mkcert certificate. This mirrors the prod
edge-proxy model and makes the dev stack reachable cross-machine on the LAN —
browsers only allow WebAuthn in a secure context, so the previous plain-HTTP
`*.localhost` topology was host-only. The dev domain is `kallipai.lan`: the
code default for `KALLIP_DOMAIN` is the prod domain (`kallipai.com`), which
`.env.example` overrides to `kallipai.lan` for local dev (copied into `.env`,
loaded into the shell by direnv's `dotenv`) so dev never clashes with
production. Caddy runs on the host network and host-routes two vhosts to
`127.0.0.1`: `app.kallipai.lan` -> the host vite dev server (`:5173`), and
`api.kallipai.lan` -> the platform's single API face, path-routed by
service under `/v1/<service>` (archeion `:7100`, lesche `:7200`, files
`:7400` pass-through, instances `:7300`). Everything a browser calls
lives on the one `api.` origin, so the session cookie needs no `Domain`
attribute and CORS collapses to the `https://app.kallipai.lan` origin
with credentials. One-time host
setup (mkcert cert + LAN DNS) and client CA trust are covered in
[development.md](../development.md). The tagma container reaches the
edge's loopback plaintext face (`http://127.0.0.1:7443`), not the
certificate-backed vhosts.

## Production

Production is split into two standalone compositions under `compose/prod/`.
Each is a flat, single-mode file; invoke it from the **repo root** (so `.env`
resolves):

### tagma — `arion -f compose/prod/tagma.nix up -d`

Brings up the tagma (agent host + in-process relay connector) from
`packages.kallip-tagma-image`. The relay connector talks to the prod-deployed
platform over the public internet through one origin (`KALLIP_POLIS_URL`,
e.g. `https://api.kallipai.com`): enrollment posts to `{origin}/v1/archeion`
— the stored tagma token is reused thereafter — and the tunnel, envelope
POSTs, and key-exchange responses go to `{origin}/v1/lesche`.

> **Note**: the data-plane relay (`kallip-lesche`) is a separate service from
> the archeion, reached over its `/internal/*` ControlPlane API guarded by a
> shared secret (the archeion-provisioned internal token on both). The
> operator's edge path-routes `/v1/archeion` and `/v1/lesche` on the single
> `api.<d>` host to the two services; `/internal` is reached by the lesche
> over the private network, never via the public edge.

```sh
arion -f compose/prod/tagma.nix up -d
arion -f compose/prod/tagma.nix logs -f
```

Secure the tagma's published `3000` port (the operator API) — do not expose it
on a public host without a firewall / TLS reverse proxy in front.

### archeion — `arion -f compose/prod/polis.nix up -d`

Brings up the archeion (from `packages.kallip-archeion-image`) + lesche (from
`packages.kallip-lesche-image`) + files (from `packages.kallip-files-image`) +
`archeion-postgres` / `lesche-postgres` / `files-postgres` (official
`postgres:17.5` image) — co-located on one host. **None of the three services
is published** — all sit behind the operator's TLS-terminating edge
proxy, which path-routes the single `api.<d>` host by service (`/v1/archeion/*` →
`archeion:7100`, `/v1/lesche/*` → `lesche:7200`, `/v1/files/*` → `files:7400`,
`/v1/instances/*` → `instances:7300`) and
sets `X-Forwarded-For`; configure
`KALLIP_ARCHEION_TRUSTED_PROXIES` to the proxy's CIDR. Secret-bearing env (DB
url, WebAuthn RP, CORS, cookie domain, admin token, the internal shared
secret) and the postgres credentials come from `.env`; each service's
operational env (listen addr, files blob root, internal hop URL) is
pinned inline in `service.environment`, which overrides `env_file`.
The operator edge route table (Caddy) for the `api.<d>` host — the formal
prod edge spec (dev's `Caddyfile.dev` mirrors it on the loopback face;
unlisted `/v1/*` prefixes are a real 404):

```caddy
api.<d> {
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

### polis services — the NixOS module

For a step-by-step walkthrough of a full host deployment — module import,
token file, and verification — see
[nixos-deployment.md](../nixos-deployment.md); this section is the
option-level reference.

The archeion, lesche, files, and instances services run as systemd units
on the NixOS host, enabled with one switch:

```nix
services.kallipai.polis = {
  enable = true;
  archeionPackage = inputs.self.packages.x86_64-linux.kallip-archeion;
  leschePackage = inputs.self.packages.x86_64-linux.kallip-lesche;
  filesPackage = inputs.self.packages.x86_64-linux.kallip-files;
  instancesPackage = inputs.self.packages.x86_64-linux.kallip-instances;
};
```

The module binds all four services to localhost (the host's reverse proxy
is the only ingress), stands up a shared PostgreSQL for the three stateful
services (per-service databases, unix-socket peer auth), orders the units
postgresql → archeion → lesche/files (hard `Requires=` on the archeion:
its consumers read the provisioned internal-token file at boot), and
wires each stateful service's `KALLIP_*_LOG_DIR` to its systemd
`LogsDirectory`. The instances proxy additionally fronts the host
daemon's socket (soft order on `kallip-daemon.service`: while the daemon
is down it answers 503 daemon_unreachable) and joins the daemon's socket
access group. Secret state follows lifetime: the archeion provisions the
platform-internal secret into `/var/lib` (0640, generated once, never
rewritten); the auto-generated admin token lives in `/run` (0600,
rewritten every start); pinned token files are administrator-owned 0600
EnvironmentFile paths (`notifyTokenFile` unset disables the files→lesche
event push). All service tuning options are nullable and default to the
binaries' own defaults — see the option descriptions in
`nix/nixos-modules.nix`.

The module binds every service to localhost; routing their public
names is the deployment's own edge configuration — see the reverse
proxy section in [nixos-deployment.md](../nixos-deployment.md) for a
copy-paste `services.caddy` example (the lesche route should flush
immediately so the event stream never buffers behind the proxy).

### web — the site root

On NixOS, `config.services.kallipai.web.distWithRuntimeConfig` is the
site root your edge serves with a plain `file_server` block: the
kallip-web bundle as-is while `services.kallipai.web.runtimeConfig` is
empty, or with a runtime config payload baked into `config.js` once
keys are set:

```nix
services.kallipai.web = {
  enable = true;
  runtimeConfig = {
    domain = "kallipai.lan";
  };
};
```

Unset keys of `runtimeConfig` fall back to the app-side derivation; the
bundle itself is deployment-independent (the flake's
`packages.kallip-web-dist`, built in two derivations: a networked deps
build and an offline vite build). The dev form of the same site is the
host vite server behind the dev Caddyfile (see the dev section above).

## Relay bootstrap

Applies to both dev and the prod-tagma composition (the only compositions that
run the tagma with a relay configured). The tagma's relay connector enrolls on
its **first** boot using `KALLIP_TAGMA_RELAY_ENROLLMENT_CODE` (a single-use
`sk-enroll-...` minted via the archeion dashboard after a user signs up). After
that it persists the tagma token under the instance data root's `credentials/` (i.e. inside the
`data` volume) and reuses it. Leave the code unset on subsequent boots. The
first-boot `enroll()` is not retried in code: on a missing/unreachable archeion it
logs an error, leaves the relay unset, and keeps serving local agents (the
lesche message route returns 503). The tagma service is
`restart: unless-stopped`, so it comes
back once the code is supplied / the archeion is reachable (check
`arion logs tagma`).

## Integration tests: `arion -f compose/dev/test.nix up`

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
arion -f compose/dev/test.nix up
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

The three compositions that run the tagma — `compose/dev/tagma.nix`,
`compose/dev/test.nix`, and `compose/prod/tagma.nix` — grant what this needs,
on the `tagma` service only:

- `service.capabilities.SYS_ADMIN = true` (→ `cap_add: [SYS_ADMIN]`)
- `out.service.security_opt = [ "seccomp=unconfined" ]`

The archeion and both postgres services need no special privileges.

Docker's default shape is rootful: the container's uid 0 is the host's
uid 0 in the initial user namespace — exactly what the guard refuses,
since every spawned instance would be a host-root process and the
sandbox stack does not reclaim uid capabilities. Prefer a rootless or
userns-remapped runtime. Where that is not an option, set
`KALLIP_TAGMA_ACCEPT_UNSAFE_RUN_AS_ROOT=1` (exact value) in the tagma
service's environment and accept the per-boot warning; anything else —
unset, empty, `0` — keeps the refusal. Inside the platform this
variable travels through the request env pairs (`kallipctl --env`),
not the daemon's own environment: the daemon forwards only what a
request carries (see the Local daemon section in [env.md](env.md)).

The grant lives on the compose `service.environment` line, not in
`.env`: the tagma services also load `.env` (via `env_file`), but an
explicit `service.environment` entry wins -- so the consent is
granted and revoked by editing the compose file, never the env file.

## Volumes and workspaces

In dev and the prod-tagma composition, tagma data and the agent workspace are
**docker named volumes** — no host directories are created and the project tree
stays clean. Shared skills live inside the `kallipai_tagma_data` volume's
`skills/` subdir, and the tagma credentials (device key + tagma token) live
under
`/var/lib/kallipai/tagmata/main/credentials/` inside that volume. The
archeion and each
postgres service add their own volumes in the compositions that run them. The
test composition mounts none (its scratch tree is an ephemeral `/testdata`
tmpfs).

- `kallipai_tagma_data` named volume → `/var/lib/kallipai/tagmata/main` — agent state, logs, skills, and the tagma credentials (persistent; survives `arion down`, removed by `arion down -v`).
- `workspace` named volume → `/workspace` — the agent workspace root.
- `archeion_pgdata` named volume → `/var/lib/postgresql/data` — the archeion's Postgres store (dev + the prod-archeion composition).
- `lesche_pgdata` named volume → `/var/lib/postgresql/data` — the lesche's Postgres chat store (dev + the prod-archeion composition).

**In dev only**, data and workspace can be bind-mounted to a host path via
their env vars, when you want the files on the host (e.g. inspect/persist tagma
state, or have the agent work on a checkout). Shared skills can be overlaid on
the data volume's `skills/` subdir the same way. Prod-tagma uses plain
named volumes — if you need tagma state on a specific disk, pin it at the
docker layer (data-root) or edit the compose:

```sh
KALLIP_ARION_DATA_PATH=$PWD/data arion up -d        # /var/lib/kallipai/tagmata/main ← host ./data
KALLIP_ARION_WORKSPACE_PATH=$PWD/ws arion up -d     # /workspace ← host ./ws
KALLIP_ARION_SKILLS_PATH=$PWD/skills arion up -d    # /var/lib/kallipai/tagmata/main/skills ← host ./skills
```

Don't point `KALLIP_ARION_SKILLS_PATH` at the same host path as
`KALLIP_ARION_DATA_PATH` — the skills subdir would shadow itself
confusingly. The override value must be an absolute, colon-free path (the
compose throws at eval otherwise).

`workspace_root` passed to the tagma API is always resolved as an in-container
path (default `/workspace`); a host bind does not change what the tagma sees.

Each agent needs a `workspace_root` that exists in the container and is
**disjoint** from `/var/lib/kallipai/tagmata/main`. Pass `workspace_root: /workspace` when
creating an agent via the [tagma API](tagma-api.md); the tagma rejects a
workspace that contains or is contained by the data dir.

## Environment

The compose sets the per-service defaults (the tagma's `KALLIP_TAGMA_ADDR`,
`HOME`, `PATH`, `RUST_LOG`, `KALLIP_WORKSPACE_ROOT`; the dev archeion's WebAuthn
RP/CORS/cookie values). Provider credentials, tokens, and the prod-archeion deploy
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
| `KALLIP_POLIS_URL`                   | **yes** (prod-tagma) | The platform edge origin (e.g. `https://api.kallipai.com`); enrollment, tunnel, and envelope posts all derive from it. (Dev sets `http://127.0.0.1:7443`, the loopback edge.) |
| `KALLIP_TAGMA_RELAY_ENROLLMENT_CODE` | first boot only      | A `sk-enroll-...` minted via the archeion dashboard. Remove after the first successful enroll.                                                                  |

Archeion + lesche + their two postgres services (the prod-archeion composition) —
`.env` only (dev derives these from `KALLIP_DOMAIN`, set to `kallipai.lan`
in `.env` via `.env.example`; the code default is the prod `kallipai.com`):

| Variable                          | Required                      | Notes                                                                                                                                  |
| --------------------------------- | ----------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `KALLIP_ARCHEION_DATABASE_URL`       | **yes** (prod-archeion)          | `postgres://<USER>:<POSTGRES_PASSWORD>@archeion-postgres:5432/<DB>` — user/db/password must match the `POSTGRES_*` vars. The archeion's identity store. |
| `KALLIP_LESCHE_DATABASE_URL`      | **yes** (prod-archeion)          | `postgres://<USER>:<POSTGRES_PASSWORD>@lesche-postgres:5432/<DB>` — the lesche's durable chat store (rooms, membership, message payloads). |
| `POSTGRES_USER`                   | **yes** (prod-archeion)          | The postgres superuser role; read by BOTH postgres services and must match the user in both `*_DATABASE_URL`s (the image defaults to `postgres`).                    |
| `POSTGRES_PASSWORD`               | **yes** (prod-archeion)          | The postgres superuser password (read by both postgres images).                                                                          |
| `POSTGRES_DB`                     | **yes** (prod-archeion)          | The initial db; read by BOTH postgres services and must match the db in both `*_DATABASE_URL`s (the image defaults to `postgres`).                                   |
| `KALLIP_ARCHEION_WEBAUTHN_RP_ID`     | **yes** (prod-archeion)          | The registrable domain passkeys bind to; cannot change without invalidating every passkey.                                             |
| `KALLIP_ARCHEION_WEBAUTHN_RP_ORIGIN` | **yes** (prod-archeion)          | The exact origin the web app is served from (`https://app.example.com`).                                                               |
| `KALLIP_ARCHEION_CORS_ORIGINS`       | **yes** (prod-archeion)          | The app origin(s); never a wildcard on a public deploy.                                                                                |
| `KALLIP_ARCHEION_COOKIE_SECURE`      | no (defaults true)            | Keep `true` behind TLS; `false` only for plain-HTTP dev. Dev is now behind Caddy's TLS and hardcodes `true`.                            |
| `KALLIP_ARCHEION_TRUSTED_PROXIES`    | **yes** behind a remote proxy | Loopback-only by default and **cleared on a public bind**; set to the proxy's CIDR so X-Forwarded-For / per-client rate limiting work. Dev trusts loopback (`127.0.0.0/8, ::1/128`) because Caddy proxies over the host network. |
| `KALLIP_ARCHEION_ADMIN_TOKEN`        | no                            | Stable admin token (the pin form). The generated form writes a fresh token to runtime state on every start and never reaches the logs. |
| `KALLIP_ARCHEION_ADMIN_TOKEN_OUT_FILE` | no (required when no token is set) | Where a generated admin token is written (0600, `KEY=value`), rewritten on every start; the NixOS module sets it to `/run/kallipai/archeion/admin-token.env`. |
| `KALLIP_ARCHEION_INTERNAL_TOKEN_FILE` | no (unset = standalone)    | Where the archeion provisions the platform-internal secret (0640, generated on first boot, never rewritten); the NixOS module sets it to `/var/lib/kallipai/archeion/internal-token`. |
| `KALLIP_POLIS_INTERNAL_TOKEN_FILE`   | **yes** (lesche / files / instances in platform mode) | File holding the archeion-provisioned internal secret, read at boot (bounded wait, then refuse to start). |
| `KALLIP_LESCHE_CORS_ORIGINS`      | **yes** (prod-archeion)          | The app origin(s) for the lesche; never a wildcard on a public deploy.                                                                 |

The archeion, lesche, files, and instances services can also be configured through the `services.kallipai.polis` NixOS module and its token files instead of `.env` (see the polis section above).

Note: unset WebAuthn RP values fall back to the kallipai.com prod pair
(passkeys simply stay unusable until configured) instead of failing boot.

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
  -v kallipai-tagma_data:/var/lib/kallipai/tagmata/main \
  -v kallipai-tagma_workspace:/workspace \
  -e KALLIP_LLM_PROVIDER=deepseek \
  -e KALLIP_LLM_MODEL=deepseek-v4-flash \
  -e KALLIP_LLM_DEEPSEEK_API_KEY="$DEEPSEEK_KEY" \
  kallip-tagma:latest kallip-tagma
```

(The `kallip-tagma-image` has no default `Cmd` — pass the binary name
`kallip-tagma` explicitly.)

The archeion runs from `kallip-archeion-image` (behind your own TLS reverse proxy +
a postgres):

```sh
nix build .#kallip-archeion-image
docker load < result
docker run --rm \
  -e KALLIP_ARCHEION_DATABASE_URL=postgres://kallip:...@postgres:5432/kallip \
  -e KALLIP_ARCHEION_WEBAUTHN_RP_ID=app.example.com \
  -e KALLIP_ARCHEION_WEBAUTHN_RP_ORIGIN=https://app.example.com \
  -e KALLIP_ARCHEION_CORS_ORIGINS=https://app.example.com \
  kallip-archeion:latest
```

Then create an agent via the [tagma API](tagma-api.md) with
`workspace_root: /workspace`.
