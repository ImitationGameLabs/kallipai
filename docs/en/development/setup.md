---
title: Development
description: Workspace layout, build commands, and the verification workflow for contributors.
order: 30
internal: true
---

Local development runs the full kallip stack under
[Arion](https://docs.hercules-ci.com/arion/) (a Nix-native docker-compose). The
dev archeion side lives at `compose/dev/polis.nix`; the repo-root
`arion-compose.nix` is a one-line shim that re-exports it for arion's
auto-discovery, so a plain `arion up` brings it up.

This doc covers the day-1 bring-up and the iteration loop. For the container
images, the production split, and the integration-test mode, see
[Container images](../deployment/container.md); for the frontend workspace, see
[Frontend package development](frontend.md).
For the NixOS host deployment, see
[the NixOS deployment guide](../deployment/nixos/index.md).

## Prerequisites

- Arion + a Docker (or Podman with the docker socket) daemon. Under rootless
  docker, the Caddy service uses host networking and binds the edge port
  (`KALLIP_EDGE_PORT`, default 443) on the host, which requires
  `sysctl net.ipv4.ip_unprivileged_port_start=80` (or
  running the daemon as root) when that port is below 1024.
- [mkcert](https://github.com/FiloSottile/mkcert) for the dev TLS
  certificate (https edge only).
- Copy `.env.example` to `.env` and fill in the LLM provider credentials. Arion
  reads `.env` via `service.env_file`.

### Plain-http Quick Start (KALLIP_EDGE_TLS=off)

Set `KALLIP_EDGE_TLS=off` in `.env` for a plain-http edge with no mkcert
and no DNS-trust setup. Keep `KALLIP_DOMAIN=localhost` (the default is
the prod domain; the quick start pins localhost) and move the edge off
the privileged default port: `KALLIP_EDGE_PORT=8080`. Then `arion up -d`

- `deno task dev` and open `http://app.localhost:8080`: browsers
resolve every `*.localhost` name to the loopback interface, so no
hosts-file entry is needed. Login surface: admin key + GitHub oauth;
passkeys work on localhost out of the box (a browser secure-context
exemption). For multi-machine access set `KALLIP_DOMAIN` to the LAN
host and resolve `*.<domain>` on your LAN DNS (passkey/Google then
stay browser-blocked on plain http).

Gotchas on this shape:

- A browser that has visited the same hostname over https keeps the old
  `Secure`-flagged `kallip_session` cookie and then refuses to store the
  new non-`Secure` one: delete the old cookie first.
- After editing `.env`, restart `deno task dev` and `arion up`: both
  read the env at startup, not per request.
- The host you browse must match `KALLIP_DOMAIN` (`localhost` here);
  any other host is rejected.

#### TLS + DNS Setup (the Default https Edge, One-Time)

The dev edge terminates TLS for `*.<devDomain>` so the stack is reachable
cross-machine on the LAN (browsers only allow WebAuthn in a secure
context, so plain HTTP + a raw LAN IP cannot work). This is a
one-time setup, and the https edge is the default shape.

The dev domain is `kallipai.lan`. The code default for `KALLIP_DOMAIN` is
the production domain (`kallipai.com`); `.env.example` sets it to
`kallipai.lan` for local dev (so dev DNS/certs never clash with
production), and you get that when you copy `.env.example` to `.env`.
direnv's `dotenv` loads `.env` into the shell, so arion eval, `mkcert`,
and vite all see it. The whole stack (the archeion/lesche env, the
Caddyfile, and vite's dev-server shaping (allowedHosts, HMR websocket)
) derives from `KALLIP_DOMAIN` plus the two edge knobs
(`KALLIP_EDGE_TLS`, `KALLIP_EDGE_PORT`). The web app's own API URLs
read none of them: they derive at runtime from the browser location
(see the offline-login notes below).

1. Generate the leaf cert with `mkcert` (provided by the nix devShell). Run this
   from the repo root; `$KALLIP_DOMAIN` comes from `.env` (`kallipai.lan`):

   ```sh
   mkdir -p compose/dev/.certs && \
     mkcert -cert-file compose/dev/.certs/cert.pem -key-file compose/dev/.certs/key.pem \
       "*.$KALLIP_DOMAIN" "$KALLIP_DOMAIN"
   ```

   This writes `cert.pem` / `key.pem` into `./compose/dev/.certs/` for
   `*.<devDomain>` + the bare domain, and creates the mkcert root CA at
   `~/.local/share/mkcert/rootCA.pem` on first use. It does **not** install the
   root into any trust store; that step is OS-specific (step 2).
   `compose/dev/polis.nix` defaults the cert dir to `<repo>/compose/dev/.certs`,
   so arion finds them with nothing further to do.
2. Install the mkcert root CA into the host trust store, so the leaf cert is
   accepted by the browser (no warning, and WebAuthn runs in a real secure
   context). This is a manual step: the `mkcert` command above does not do it:

   ```sh
   mkcert -install
   ```

   **NixOS caveat:** `mkcert -install` does NOT work: the system store and
   Java `cacerts` live in the read-only `/nix/store`, so mkcert can't mutate
   them in place. Add the root to the system trust via config and rebuild
   instead (this also feeds the browser NSS/p11-kit and Java `cacerts` stores):

   ```nix
   security.pki.certificateFiles = [
     "/home/<you>/.local/share/mkcert/rootCA.pem"
   ];
   ```

3. Resolve `*.<devDomain>` to the host's LAN IP. This is **host/LAN
   infrastructure, not part of the dev stack**: keep it out of the arion
   composition and configure it wherever your network DNS lives. The exact
   mechanism depends on your host OS / network; pick one:

   - **Quickest (one or two clients only):** add an entry to each client's
     `/etc/hosts`:

     ```text
     192.168.1.7  app.kallipai.lan api.kallipai.lan
     ```

     `/etc/hosts` does not support wildcard entries, so list each subdomain
     explicitly.

   - **A LAN resolver** (dnsmasq / AdGuard Home / Pi-hole, often on a router or
     NAS); best when several devices need to reach the dev stack. Add a
     wildcard record `*.kallipai.lan` -> the host's LAN IP, then either
     advertise that resolver over DHCP or point each client at it manually. On a
     NixOS host, for example:

     ```nix
     services.dnsmasq = {
       enable = true;
       settings.address = [ "/kallipai.lan/192.168.1.7" ]; # <- the host's LAN IP
     };
     ```

     On macOS or a non-NixOS Linux host, the same dnsmasq / AdGuard Home /
     Pi-hole services run just as well (install via Homebrew, apt, etc.), or
     use your router's built-in DNS.

   Whatever you choose, the goal is the same: a browser on the client device
   resolves every `*.<devDomain>` name to the host running the arion stack.
4. On each **client device** that will open the app (e.g. another laptop on the
   LAN), install the mkcert root CA (`mkcert -CAROOT` prints the path; copy
   `rootCA.pem` into the device's trust store). Tauri Android/iOS builds need
   the root trusted at the OS level, which is more involved.

> **Scope:** this topology covers the **web** app (`packages/kallip-web`). The
> Tauri Android shell (`packages/kallip-app`) is a separate target that still
> defaults to `http://localhost:7100` / `:7200` and is not wired to the
> `*.kallipai.lan` dev cert; see [Frontend package development](frontend.md).

### Bring-Up

The stack comes up in two phases because the tagma's relay connector cannot
enroll with the archeion until a real user signs up in the web UI and mints an
enrollment code -- starting it with `KALLIP_POLIS_URL` set but no code
degrades the tagma to local-only (it logs an error and keeps serving local
agents; the lesche message route returns 503).

#### Archeion Side

```sh
arion up -d                # caddy + archeion + lesche + files + instances + archeion-postgres + lesche-postgres + files-postgres (arion builds the workspace via the flake)
```

Dev is fronted by Caddy (see the one-time setup above): the browser loads the
web app at `https://app.kallipai.lan` and calls the platform at
`https://api.kallipai.lan`, all TLS-terminated by Caddy. Everything shares
the one origin pair, so the session cookie stays first-party. The web app
(`deno task dev` from `packages/kallip-web`) derives its API base in the
browser from the origin it runs on: `https://app.kallipai.lan` yields
`https://api.kallipai.lan`, and every client hangs its templates off the
`/v1/<service>` paths under that base. The derived URLs already match the
Caddy topology, so no `.env` override is needed for normal LAN dev;

archeion and lesche also publish `7100` / `7200` to the host for plain-HTTP
tooling: `kallip-admin` and curl keep using `http://localhost:7100` /
`http://localhost:7200` directly, bypassing Caddy. The files service
publishes `7400` on the loopback interface only; the browser reaches files
only through the edge (the edge strips `/v1/files`), while the `kallip file`
CLI points `KALLIP_POLIS_URL` at the dev edge (`https://api.kallipai.lan`) and presents a
tagma bearer (`KALLIP_FILES_TOKEN`); see docs/en/reference/files-api.md.
The tagma process itself authenticates to the files service with its
registered enrollment credential: `KALLIP_FILES_TOKEN` provisions
CLI shells, and the tagma removes a leftover copy from its own
environment at boot.

> **Passkeys and the RP id:** if a dev environment was ever brought up on
> the old `localhost` topology, its registered passkeys are invalid under
> the `kallipai.lan` RP id: reset the archeion volume (`arion down -v`)
> and re-register before the first login.

##### Register a Test User (First Bring-Up Only)

Signup is open (no invite code): a fresh database just needs someone to sign
up. The `archeion_pgdata` volume persists across `arion down` / `up`, so this
sub-flow runs **once per volume** -- check before doing it:

```sh
KALLIP_ARCHEION_ADMIN_TOKEN=sk-admin-dev-0123456789abcdef0123456789abcdef cargo run -q -p kallip-admin -- --archeion-url http://localhost:7100 users list
```

If `users list` already shows a row, a test account exists -- skip to minting
the enrollment code below. If the table is empty (fresh volume, or after a
`down -v` reset), sign up at the web app:

Open the web app at `https://app.kallipai.lan` and sign up with a username +
passkey (or "Continue with GitHub/Google" once OAuth is configured). Once signed
in, mint a `sk-enroll-...` enrollment code in the web UI and paste it into
`.env` as `KALLIP_TAGMA_RELAY_ENROLLMENT_CODE`, and set `KALLIP_AUTH_TOKEN` to
the tagma's operator token. (The tagma's platform edge origin is wired by arion
to the loopback edge -- `http://127.0.0.1:7443` -- so it
needs no `.env` override.)

Faster alternative without touching a browser: the dev stack enables the
local-platform admin-login, so a plain curl exchanges the admin token for a
User session on a fixed `admin` account (created on first use) and that
session mints enrollment codes like any user:

```sh
curl -si -X POST http://localhost:7100/auth/admin-login \
  -H 'Authorization: Bearer sk-admin-dev-0123456789abcdef0123456789abcdef'
```

The `Set-Cookie: kallip_session=...` header is the session (see
docs/en/reference/auth.md); pass it as `-b kallip_session=...` to mint an
enrollment code at `POST /tagmata` on the same direct port (no
`/v1/archeion` prefix) without signing up.

###### The admin token

`kallip-admin` authenticates with the archeion's admin token. The clean path is to
pin it **before** first boot so the same known value works on every run: make
sure `.env` contains

```text
KALLIP_ARCHEION_ADMIN_TOKEN=sk-admin-dev-0123456789abcdef0123456789abcdef
```

then run `arion up -d`. The dev compose pins this same fixture in the archeion
service's environment (a local-platform login is enabled there, and that
route refuses to boot with an operator-set token shorter than 32 chars),
so `kallip-admin` authenticates with it -- no log scraping.

If the archeion is **already running** without this pinned (e.g. an older stack
booted before you set it), its token was generated randomly at startup and
cannot be changed short of recreating the container. Either `arion up -d` to
recreate it with the pinned value, or fall back to grepping the current token
out of **archeion's** logs (not tagma's):

```sh
TOK=$(arion logs archeion 2>&1 | grep -oP 'sk-admin-[A-Za-z0-9_-]+' | tail -1)
KALLIP_ARCHEION_ADMIN_TOKEN="$TOK" cargo run -q -p kallip-admin -- --archeion-url http://localhost:7100 ...
```

The fixture is dev-only; prod must set a strong secret.

#### Tagma Side

The tagma (agent host + in-process relay connector) is a separate composition
(`compose/dev/tagma.nix`) so its lifecycle does not entangle with the archeion
side. It runs on the host network and reaches the platform edge at
`http://127.0.0.1:7443` (the loopback plaintext face), so bring the archeion side up first, then:

```sh
arion -f compose/dev/tagma.nix up -d   # tagma; enrolls its relay
```

#### Multi-Edge Tagma (Multi-Relay)

The tagma can hold one identity per platform deployment simultaneously (e.g. the local
dev edge plus a remote one). Declare the entries in
`<data-root>/polis.toml`:

```toml
[[polis]]
name    = "main"                # slug: [a-z0-9][a-z0-9-]*; keys the
                                # credentials/<name>/ dir (stable across origin changes)
url = "https://api.kallipai.com"   # the platform edge origin
# enrollment_code = "sk-enroll-..."      # first run only; afterwards the stored token is reused

[[polis]]
name     = "second"
url = "https://relay2.example.com"
```

Rules: a `polis.toml` entry and the env-configured single relay
(`KALLIP_POLIS_URL`, optionally with `KALLIP_TAGMA_RELAY_ENROLLMENT_CODE`)
are mutually exclusive -- unset the env vars or delete the file (the env
vars keep working as one implicit `default` entry when the file is
absent). Each entry enrolls with its own edge identity
(`credentials/<name>/tagma.id` + `tagma.token`); the Ed25519 `device.key`
at the credentials root is shared (one device, many identities). A flat
`credentials/tagma.id` (single-entry layouts) is migrated into the
entry's directory on the first boot; anything ambiguous fails fast with
both origins named. Outbound frames fan out to every online relay; the
first entry with stored credentials is the "primary" edge (frontend
cache key).

Multi-edge acceptance runs on a second, parallel stack: the same compose
file parameterized by env vars:
`KALLIP_ARION_PROJECT_NAME=kallipai-dev2 KALLIP_ARION_ARCHEION_PORT=7101 KALLIP_ARION_LESCHE_PORT=7201 arion up -d archeion lesche`
-- with its own containers, volumes, and edge: the second stack's caddy
routes its own `api.` host to those ports, giving the tagma a second
platform origin. Enroll a code on each side, fill `polis.toml`, and watch the
tagma log for two `relay connector active` lines (one per entry name); a
message sent on either side must arrive on both.

#### Manual KEX Round-Trip Acceptance

The automated acceptance chain covers fanout and dual identity; the
user-agent KEX round-trip itself (message in, agent reply out, both sides
receiving) is a manual residual item. Verify it by hand once per dual-archeion
setup:

1. Bring the archeion side and the web dev server up (`deno task dev` from
   `packages/kallip-web`), open `https://app.kallipai.lan/register`, and
   create a user (username + passkey).
2. Open `https://app.kallipai.lan/tagmata`, pick the enrolled tagma, send a
   message, and wait for the agent reply.
3. Confirm the exchange landed on both lesche instances. Message bodies
   are E2E ciphertext, so compare rows, not content:

   ```sh
   for c in kallipai-dev-lesche-postgres-1 kallipai-dev2-lesche-postgres-1; do
     docker exec "$c" psql -U kallip -d kallip -c \
       'select room_id, seq, sender_kind, created_at
        from room_messages order by created_at desc limit 4'
   done
   ```

Both sides must list the new rows: `human` for the user's message,
`agent` for the reply.

#### Local Daemon Management (kallipctl)

The daemon family (`crates/daemon/`) manages multiple local tagma
instances. The daemon keeps one registration record per instance in
its record area (default `~/.local/state/kallipai/daemon/instances/`,
one `<slug>.json` per instance); the record and the instance's own
`runtime.json` are the only truth. `kallipctl` talks to it over a
0600 control socket (0660 group-widened when
`KALLIP_DAEMON_SOCKET_GROUP` is set; the system form):

```sh
kallipctl spawn <slug> <workspace> -e KALLIP_LLM_PROVIDER=... \
    -e KALLIP_LLM_MODEL=... -e KALLIP_LLM_DEEPSEEK_API_KEY=...
kallipctl spawn <slug> <workspace> --user <account>  # drop to a pre-declared user
kallipctl list           # every record in the record area
kallipctl health <slug>  # pid liveness via /proc/<pid>/comm
kallipctl stop <slug>    # TERM, 10s grace, KILL
```

The record carries the instance id, owning uid, workspace, user env,
launch anchor, and the pointer at the data directory
(`~/.local/share/kallipai/tagmata/<slug>`); the tagma publishes
`runtime.json` (pid + port + starttime) into the data directory on
every boot. Env
pairs must start with `KALLIP_` or be `RUST_LOG` or `PATH`; the reserved
keys (`KALLIP_TAGMA_SLUG`,
`KALLIP_WORKSPACE_ROOT`, `KALLIP_TAGMA_DATA_DIR`) are daemon-owned.
`KALLIP_TAGMA_ADDR` is the one user-set listen knob: the daemon injects
`127.0.0.1:0` unless any channel carries the key: a request pair,
the harvested base, or the persisted record replayed on start; every
channel is shape-checked as a `SocketAddr`,
and a pinned port already in use only surfaces at bind time; spawn
times out and rolls the record back; start keeps the record and
reports the timeout pointing at the logs.
A `start` relaunch drops `KALLIP_TAGMA_RELAY_ENROLLMENT_CODE` from the
replayed env once the instance holds stored relay credentials
(`credentials/default/`) and scrubs it from the record in the same stroke:
the code is single-use, and replaying it after a completed enrollment trips
tagma's conflicting-relay-configuration fail-fast. An instance whose
enrollment never completed still replays the code, so a restart can retry.
A daemon-managed tagma instance logs into the state tree:
`~/.local/state/kallipai/tagmata/<slug>/logs/` (created 0700): daily-rolling
files, the last 7 kept. Set `KALLIP_TAGMA_LOG_TO_STDERR=1`
(or `true`) to log to the terminal's stderr instead -- handy when manually
debugging a managed data dir; any other value keeps the file default, and
a log directory that cannot be resolved or created falls back to stderr.
The web management face lives in `crates/platform/kallip-instances`: a
pure JSON API at the root, proxying the daemon over its
UDS socket. Platform mode: the archeion's internal face
verifies the SPA's `sk-admin-` key (the operator-key login). The SPA
itself is served by the host vite dev server (Caddy routes
`app.<devDomain>` to `:5173`) and calls the API cross-origin from the
web origin. The operator-key login branch is runtime-config gated: the
shipped `config.js` factory default is `offlineLogin = true` (the
self-hosted posture), and a cloud-facing deployment hides the branch by
setting `services.kallipai.web.runtimeConfig.offlineLogin = false`
(baked into the served site root), or by editing the file directly on a
non-NixOS deployment.

### Iterating

`arion up` re-evaluates the flake each time, so Rust changes are picked up just
by running it again -- arion builds the workspace transitively (via the image
contents) and `useHostStore` shares that `/nix/store` into the containers:

```sh
arion up -d                                # archeion side
arion -f compose/dev/tagma.nix up -d       # tagma side, if you want it up
```

Tail logs with `arion logs -f <service>` (`archeion`, `archeion-postgres`,
`lesche-postgres`); for the tagma use `arion -f compose/dev/tagma.nix logs -f
tagma`.

### Optional Bind Overrides

By default the tagma data, the agent workspace, and shared skills live in
docker volumes. Set these env vars (absolute, colon-free host paths) to
bind-mount them on the host instead:

| Env var                       | Mounts                   | Use case                            |
| ----------------------------- | ------------------------ | ----------------------------------- |
| `KALLIP_ARION_DATA_PATH`      | `/var/lib/kallipai/tagmata/main` | keep tagma state on a known disk    |
| `KALLIP_ARION_WORKSPACE_PATH` | `/workspace`             | make the agent's files host-visible |
| `KALLIP_ARION_SKILLS_PATH`    | `/var/lib/kallipai/tagmata/main/skills` | curate shared skills on the host    |

Leave `KALLIP_SKILLS_ROOT` unset when using `KALLIP_ARION_SKILLS_PATH` -- the
former redirects `skill_dir()` away from the bind-mount target.

### Integration Tests

Runs the workspace's `[[test]]` targets **inside the container** to confirm the
sandbox and shell backends behave in the containerized environment the tagma
ships in; the service exits with the overall verdict (`arion ps -a`).

```sh
arion -f compose/dev/test.nix up
```

The suites today: the sandbox suite (a scripted end-to-end agent driving
  real landlock + seccomp + mount-ns shell sandbox) and the exec suite (real
  `bash -c` cwd and process-group behavior); any `[[test]]` target added
  later is picked up automatically.

### Reset (Clean Slate)

When the backend changes in a way that invalidates existing data (a schema
reset, an incompatible wire format, or you simply want to start over), tear down
**including volumes** and re-run bring-up from the archeion side. `down -v` wipes
`archeion_pgdata` and `lesche_pgdata` plus the tagma `data` / `workspace` volumes,
so the test user, the enrollment code, and all tagma state are gone -- the
sign-up sub-flow is needed again:

```sh
arion down -v                                  # archeion side: stop AND delete volumes
arion -f compose/dev/tagma.nix down -v         # tagma side: stop AND delete volumes
arion up -d                                    # archeion side
# ...sign up, mint enrollment code in the web UI, fill .env...
arion -f compose/dev/tagma.nix up -d           # tagma side
```

The archeion side and the tagma are separate compose projects (`kallipai-dev` and
`kallipai-dev-tagma`), so each `down -v` is scoped to its own volumes. To keep
tagma state, back up or bind-mount the volumes (see "Optional bind overrides")
instead of relying on `down -v`.
