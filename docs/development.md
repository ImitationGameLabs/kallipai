# Development

Local development runs the full kallip stack under
[Arion](https://docs.hercules-ci.com/arion/) (a Nix-native docker-compose). The
composition lives at `arion-compose.nix` at the repo root.

This doc covers the day-1 bring-up and the iteration loop. For the container
images, the production split, and the integration-test mode, see
[container.md](reference/container.md); for the frontend workspace, see
[frontend-development.md](frontend-development.md).

## Prerequisites

- Arion + a Docker (or Podman with the docker socket) daemon.
- Copy `.env.example` to `.env` and fill in the LLM provider credentials. Arion
  reads `.env` via `service.env_file`.

## Bring-up

The stack comes up in two phases because the tagma's relay connector cannot
enroll with the agora until a real user signs up in the web UI and mints an
enrollment code -- starting it with `KALLIP_TAGMA_RELAY_AGORA_URL` set but no code
degrades the tagma to local-only (it logs an error and keeps serving local
agents; the lesche message route returns 503).

### Phase 1 -- agora side

```sh
arion up -d                # agora + lesche + postgres (arion builds the workspace via the flake)
```

Dev uses a per-service subdomain topology with no edge proxy: the web app
reaches the agora at `http://agora.localhost:7100` and the lesche at
`http://lesche.localhost:7200` (browsers resolve `*.localhost` natively). The
session cookie carries `Domain=localhost` so it is shared across the two
subdomains. The web app (`deno task dev` from `packages/kallip-web`, served at
`:5173`) must therefore configure **two API origins** and send
`credentials: "include"` on its requests to both — CORS on each service already
allows `http://localhost:5173`. The web app reads the two origins from
`VITE_AGORA_URL` (default `http://localhost:7100`) and `VITE_LESCHE_URL` (default
`http://localhost:7200`); set both in `.env` to the subdomain form
(`http://agora.localhost:7100` / `http://lesche.localhost:7200`) if you prefer
host-separated origins (the `Domain=localhost` cookie is shared either way).

#### Register a test user (first bring-up only)

Signup is invite-gated, so a fresh database needs an invite code before anyone
can register. The `pgdata` volume persists across `arion down` / `up`, so this
sub-flow runs **once per volume** -- check before doing it:

```sh
KALLIP_AGORA_ADMIN_TOKEN=sk-admin-test kallip-admin --agora-url http://localhost:7100 users list
```

If `users list` already shows a row, a test account exists -- skip to minting
the enrollment code below. If the table is empty (fresh volume, or after a
`down -v` reset), mint an invite code and register:

```sh
KALLIP_AGORA_ADMIN_TOKEN=sk-admin-test kallip-admin --agora-url http://localhost:7100 invite-codes new
# -> sk-invite-...   (plaintext returned once; the server stores only its hash)
```

Open the web app at `:5173` and sign up using that `sk-invite-...` code. Once
signed in, mint a `sk-enroll-...` enrollment code in the web UI and paste it
into `.env` as `KALLIP_TAGMA_RELAY_ENROLLMENT_CODE` (and set
`KALLIP_TAGMA_RELAY_AGORA_URL` / `KALLIP_TAGMA_RELAY_LESCHE_URL` to the dev subdomains), and
set `KALLIP_AUTH_TOKEN` to the tagma's operator token.

##### The admin token

`kallip-admin` authenticates with the agora's admin token. The clean path is to
pin it **before** first boot so the same known value works on every run: make
sure `.env` contains

```text
KALLIP_AGORA_ADMIN_TOKEN=sk-admin-test
```

then run `arion up -d`. The agora loads it via `env_file` and `kallip-admin`
always authenticates with `sk-admin-test` -- no log scraping.

If the agora is **already running** without this pinned (e.g. an older stack
booted before you set it), its token was generated randomly at startup and
cannot be changed short of recreating the container. Either `arion up -d` to
recreate it with the pinned value, or fall back to grepping the current token
out of **agora's** logs (not tagma's):

```sh
TOK=$(arion logs agora 2>&1 | grep -oP 'sk-admin-[A-Za-z0-9_-]+' | tail -1)
KALLIP_AGORA_ADMIN_TOKEN="$TOK" kallip-admin --agora-url http://localhost:7100 ...
```

`sk-admin-test` is a dev-only fixture; prod must set a strong secret.

### Phase 2 -- tagma side

The tagma service (agent host + in-process relay connector) is gated behind the
`tagma` profile. arion's CLI has no `--profile` flag; activate it via the
docker-compose env var:

```sh
COMPOSE_PROFILES=tagma arion up -d   # adds the tagma service; it enrolls its relay
```

## Iterating

`arion up` re-evaluates the flake each time, so Rust changes are picked up just
by running it again -- arion builds the workspace transitively (via the image
contents) and `useHostStore` shares that `/nix/store` into the containers:

```sh
arion up -d                           # agora side
COMPOSE_PROFILES=tagma arion up -d    # tagma side, if you want it up
```

Tail logs with `arion logs -f <service>` (`agora`, `tagma`, `postgres`).

## Optional bind overrides

By default the tagma data, the agent workspace, and shared skills live in
docker volumes. Set these env vars (absolute, colon-free host paths) to
bind-mount them on the host instead:

| Env var                       | Mounts                   | Use case                            |
| ----------------------------- | ------------------------ | ----------------------------------- |
| `KALLIP_ARION_DATA_PATH`      | `/var/lib/kallip`        | keep tagma state on a known disk    |
| `KALLIP_ARION_WORKSPACE_PATH` | `/workspace`             | make the agent's files host-visible |
| `KALLIP_ARION_SKILLS_PATH`    | `/var/lib/kallip/skills` | curate shared skills on the host    |

Leave `KALLIP_SKILLS_ROOT` unset when using `KALLIP_ARION_SKILLS_PATH` -- the
former redirects `skill_dir()` away from the bind-mount target.

## Integration tests

Runs the workspace's `[[test]]` targets **inside the container** to confirm the
sandbox and shell backends behave in the containerized environment the tagma
ships in; the service exits with the overall verdict (`arion ps -a`).

```sh
KALLIP_ARION_MODE=test arion up
```

See [container.md](reference/container.md) for which suites run.

## Reset (clean slate)

When the backend changes in a way that invalidates existing data (a schema
reset, an incompatible wire format, or you simply want to start over), tear down
**including volumes** and re-run bring-up from Phase 1. `down -v` wipes `pgdata`
plus the tagma `data` / `workspace` volumes, so the test user, the enrollment
code, and all tagma state are gone -- the invite-code sub-flow is needed again:

```sh
COMPOSE_PROFILES=tagma arion down -v   # stop everything AND delete volumes
arion up -d                            # agora side (Phase 1)
# ...mint invite code, register, mint enrollment code, fill .env...
COMPOSE_PROFILES=tagma arion up -d     # tagma side (Phase 2)
```

`COMPOSE_PROFILES=tagma` is set on the `down` too so the tagma container is
included in the teardown. Volume removal is scoped to the project as a whole,
so `down -v` wipes every named volume regardless of which profile is active --
to keep tagma state, back up or bind-mount the volumes (see "Optional bind
overrides") instead of relying on `down -v`.
