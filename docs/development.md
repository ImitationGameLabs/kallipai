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

The stack comes up in two phases because the herald cannot enroll until a real
user signs up in the web UI and mints an enrollment code -- starting the herald
with no code crashloops it.

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
host-separated origins (the `Domain=localhost` cookie is shared either way). Open
the web app at `:5173`, sign up, and mint a `sk-enroll-...` enrollment code.
Paste it into `.env` as `KALLIP_HERALD_ENROLLMENT_CODE`, and set
`KALLIP_AUTH_TOKEN` to the tagma's operator token.

### Phase 2 -- tagma side

The tagma service + herald are gated behind the `tagma` profile. arion's CLI has no
`--profile` flag; activate it via the docker-compose env var:

```sh
COMPOSE_PROFILES=tagma arion up -d   # adds the tagma service + herald; the herald enrolls
```

## Iterating

`arion up` re-evaluates the flake each time, so Rust changes are picked up just
by running it again -- arion builds the workspace transitively (via the image
contents) and `useHostStore` shares that `/nix/store` into the containers:

```sh
arion up -d                           # agora side
COMPOSE_PROFILES=tagma arion up -d    # tagma side, if you want it up
```

Tail logs with `arion logs -f <service>` (`agora`, `tagma`, `herald`,
`postgres`).

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
former short-circuits `skill_dir()` and bypasses the bind.

## Integration tests

Runs the workspace's `[[test]]` targets **inside the container** to confirm the
sandbox and shell backends behave in the containerized environment the tagma
ships in; the service exits with the overall verdict (`arion ps -a`).

```sh
KALLIP_ARION_MODE=test arion up
```

See [container.md](reference/container.md) for which suites run.
