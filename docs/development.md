# Development

Local development runs the full kallip stack under
[Arion](https://docs.hercules-ci.com/arion/) (a Nix-native docker-compose). The
dev agora side lives at `compose/dev/agora.nix`; the repo-root
`arion-compose.nix` is a one-line shim that re-exports it for arion's
auto-discovery, so a plain `arion up` brings it up.

This doc covers the day-1 bring-up and the iteration loop. For the container
images, the production split, and the integration-test mode, see
[container.md](reference/container.md); for the frontend workspace, see
[frontend-development.md](frontend-development.md).

## Prerequisites

- Arion + a Docker (or Podman with the docker socket) daemon. Under rootless
  docker, the Caddy service uses host networking and binds `:80`/`:443` on the
  host, which requires `sysctl net.ipv4.ip_unprivileged_port_start=80` (or
  running the daemon as root).
- [mkcert](https://github.com/FiloSottile/mkcert) for the dev TLS certificate.
- Copy `.env.example` to `.env` and fill in the LLM provider credentials. Arion
  reads `.env` via `service.env_file`.

### TLS + DNS setup (one-time)

The dev stack is fronted by Caddy, which terminates TLS for `*.<devDomain>` so
the stack is reachable cross-machine on the LAN (browsers only allow WebAuthn
in a secure context, so plain HTTP + a raw LAN IP cannot work). This is a
one-time setup.

The dev domain is `kallipai.lan`. The code default for `KALLIP_DEV_DOMAIN` is
the production domain (`kallipai.com`); `.env.example` sets it to `kallipai.lan`
for local dev (so dev DNS/certs never clash with production), and you get that
when you copy `.env.example` to `.env`. direnv's `dotenv` loads `.env` into the
shell, so arion eval, `mkcert`, and vite all see it. Override further
by editing `.env` or exporting `KALLIP_DEV_DOMAIN` in your shell. The whole
stack — the agora/lesche env, the Caddyfile, vite, and the web app's API URLs —
derives from this one variable.

1. Generate the leaf cert with `mkcert` (provided by the nix devShell). Run this
   from the repo root — `$KALLIP_DEV_DOMAIN` comes from `.env` (`kallipai.lan`):

   ```sh
   mkdir -p compose/dev/.certs && \
     mkcert -cert-file compose/dev/.certs/cert.pem -key-file compose/dev/.certs/key.pem \
       "*.$KALLIP_DEV_DOMAIN" "$KALLIP_DEV_DOMAIN"
   ```

   This writes `cert.pem` / `key.pem` into `./compose/dev/.certs/` for
   `*.<devDomain>` + the bare domain, and creates the mkcert root CA at
   `~/.local/share/mkcert/rootCA.pem` on first use. It does **not** install the
   root into any trust store — that step is OS-specific (step 2).
   `compose/dev/agora.nix` defaults the cert dir to `<repo>/compose/dev/.certs`,
   so arion finds them with nothing further to do.
2. Install the mkcert root CA into the host trust store, so the leaf cert is
   accepted by the browser (no warning, and WebAuthn runs in a real secure
   context). This is a manual step — the `mkcert` command above does not do it:

   ```sh
   mkcert -install
   ```

   **NixOS caveat:** `mkcert -install` does NOT work — the system store and
   Java `cacerts` live in the read-only `/nix/store`, so mkcert can't mutate
   them in place. Add the root to the system trust via config and rebuild
   instead (this also feeds the browser NSS/p11-kit and Java `cacerts` stores):

   ```nix
   security.pki.certificateFiles = [
     "/home/<you>/.local/share/mkcert/rootCA.pem"
   ];
   ```

3. Resolve `*.<devDomain>` to the host's LAN IP. This is **host/LAN
   infrastructure, not part of the dev stack** — keep it out of the arion
   composition and configure it wherever your network DNS lives. The exact
   mechanism depends on your host OS / network; pick one:

   - **Quickest (one or two clients only):** add an entry to each client's
     `/etc/hosts`:

     ```text
     192.168.1.7  web.kallipai.lan agora.kallipai.lan lesche.kallipai.lan
     ```

     `/etc/hosts` does not support wildcard entries, so list each subdomain
     explicitly.

   - **A LAN resolver** (dnsmasq / AdGuard Home / Pi-hole, often on a router or
     NAS) — best when several devices need to reach the dev stack. Add a
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
> `*.kallipai.lan` dev cert — see [frontend-development.md](frontend-development.md).

## Bring-up

The stack comes up in two phases because the tagma's relay connector cannot
enroll with the agora until a real user signs up in the web UI and mints an
enrollment code -- starting it with `KALLIP_TAGMA_RELAY_AGORA_URL` set but no code
degrades the tagma to local-only (it logs an error and keeps serving local
agents; the lesche message route returns 503).

### Agora side

```sh
arion up -d                # caddy + agora + lesche + agora-postgres + lesche-postgres (arion builds the workspace via the flake)
```

Dev is fronted by Caddy (see the one-time setup above): the browser loads the
web app at `https://web.kallipai.lan` and reaches the agora at
`https://agora.kallipai.lan` and the lesche at `https://lesche.kallipai.lan`,
all TLS-terminated by Caddy. The session cookie carries `Domain=kallipai.lan`
so it is shared across the agora/lesche subdomains. The web app (`deno task dev`
from `packages/kallip-web`) reads its two API origins from `VITE_AGORA_URL`
(default `https://agora.kallipai.lan`) and `VITE_LESCHE_URL` (default
`https://lesche.kallipai.lan`); the defaults already match the Caddy topology,
so no `.env` override is needed for normal LAN dev.

agora and lesche also publish `7100` / `7200` to the host for plain-HTTP
tooling — `kallip-admin` and curl keep using `http://localhost:7100` /
`http://localhost:7200` directly, bypassing Caddy.

> **Passkey migration:** changing the WebAuthn RP id from the old `localhost`
> topology to `kallipai.lan` invalidates every previously registered dev
> passkey. On first bring-up after this change, reset the agora volume
> (`arion down -v`) and re-register.

#### Register a test user (first bring-up only)

Signup is open (no invite code): a fresh database just needs someone to sign
up. The `agora_pgdata` volume persists across `arion down` / `up`, so this
sub-flow runs **once per volume** -- check before doing it:

```sh
KALLIP_AGORA_ADMIN_TOKEN=sk-admin-test cargo run -q -p kallip-admin -- --agora-url http://localhost:7100 users list
```

If `users list` already shows a row, a test account exists -- skip to minting
the enrollment code below. If the table is empty (fresh volume, or after a
`down -v` reset), sign up at the web app:

Open the web app at `https://web.kallipai.lan` and sign up with a username +
passkey (or "Continue with GitHub/Google" once OAuth is configured). Once signed
in, mint a `sk-enroll-...` enrollment code in the web UI and paste it into
`.env` as `KALLIP_TAGMA_RELAY_ENROLLMENT_CODE`, and set `KALLIP_AUTH_TOKEN` to
the tagma's operator token. (The tagma's agora/lesche relay URLs are wired to
compose DNS by arion -- `http://agora:7100` / `http://lesche:7200` -- so they
need no `.env` override.)

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
KALLIP_AGORA_ADMIN_TOKEN="$TOK" cargo run -q -p kallip-admin -- --agora-url http://localhost:7100 ...
```

`sk-admin-test` is a dev-only fixture; prod must set a strong secret.

### Tagma side

The tagma (agent host + in-process relay connector) is a separate composition
(`compose/dev/tagma.nix`) so its lifecycle does not entangle with the agora
side. It runs on the host network and reaches the agora/lesche at
`127.0.0.1:7100` / `:7200`, so bring the agora side up first, then:

```sh
arion -f compose/dev/tagma.nix up -d   # tagma; enrolls its relay
```

## Iterating

`arion up` re-evaluates the flake each time, so Rust changes are picked up just
by running it again -- arion builds the workspace transitively (via the image
contents) and `useHostStore` shares that `/nix/store` into the containers:

```sh
arion up -d                                # agora side
arion -f compose/dev/tagma.nix up -d       # tagma side, if you want it up
```

Tail logs with `arion logs -f <service>` (`agora`, `agora-postgres`,
`lesche-postgres`); for the tagma use `arion -f compose/dev/tagma.nix logs -f
tagma`.

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
arion -f compose/dev/test.nix up
```

See [container.md](reference/container.md) for which suites run.

## Reset (clean slate)

When the backend changes in a way that invalidates existing data (a schema
reset, an incompatible wire format, or you simply want to start over), tear down
**including volumes** and re-run bring-up from the agora side. `down -v` wipes
`agora_pgdata` and `lesche_pgdata` plus the tagma `data` / `workspace` volumes,
so the test user, the enrollment code, and all tagma state are gone -- the
sign-up sub-flow is needed again:

```sh
arion down -v                                  # agora side: stop AND delete volumes
arion -f compose/dev/tagma.nix down -v         # tagma side: stop AND delete volumes
arion up -d                                    # agora side
# ...sign up, mint enrollment code in the web UI, fill .env...
arion -f compose/dev/tagma.nix up -d           # tagma side
```

The agora side and the tagma are separate compose projects (`kallipai-dev` and
`kallipai-dev-tagma`), so each `down -v` is scoped to its own volumes. To keep
tagma state, back up or bind-mount the volumes (see "Optional bind overrides")
instead of relying on `down -v`.
