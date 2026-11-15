---
title: Container images
description: Published container images and how to run them.
order: 10
internal: true
---

The platform's services ship as **scratch-based** container images built with
`nixpkgs.dockerTools`: no Dockerfiles; each image embeds the nix store
closure of its binaries. Only `x86_64-linux` images are published. Two of
them have no compose service of their own:

- `packages.kallipai-tagma-image`: the tagma service (the agent host with its
  relay connector) plus the `kallip` CLI and the tagma's shell toolset
  (bash, coreutils, ripgrep, git, pgrep, kill). No baked-in configuration:
  whatever starts it supplies the command and the environment.
- `packages.kallipai-archeion-image`: the platform's control-plane service:
  the service binary and the CA bundle, nothing else.

The platform side runs from one compose pair in `compose/`: `compose.yaml`
plus `compose.dev.yaml` for a workstation or `compose.prod.yaml` for a host
that serves users. `scripts/compose-up.sh` is the entry point; it builds and
loads what the chosen mode runs, then brings the stack up.

| Stack    | Command                       | Services                                         |
| -------- | ----------------------------- | ------------------------------------------------ |
| **dev**  | `scripts/compose-up.sh dev`   | edge + platform services + their postgres stores |
| **prod** | `scripts/compose-up.sh prod`  | the same services, from images built for them    |
| **test** | `scripts/integration-test.sh` | the integration suites                           |

The tagma is not part of either stack: it runs on its own host, started by
whatever that host provides.

## Prerequisites

A Docker daemon with compose v2, and nix on the machine that builds: the
default path builds every image locally and loads it into the daemon, so no
registry is needed. The plain-docker flow is in
[Plain Docker](#plain-docker).

Copy `compose/.env.example` to `compose/.env` and fill in the stack's own
values; the compose files interpolate that file beside them. The relay
credentials and the daemon's own settings live in the root `.env`, which a
tagma host copies from the root `.env.example`. A prod deployment also fills
the service env file `KALLIPAI_ENV_FILE` names, whose keys
`compose/.env.prod.example` lists. Model configuration is a file instead, see
[Model Configuration](#model-configuration).

### Development Compositions

The dev bring-up is a two-phase flow (the relay connector cannot enroll until
a user has signed up and minted an enrollment code); the commands and the
iteration loop are documented in the
[development setup guide](../development/setup.md). This section covers the
dev-only mechanics: `useHostStore` bind-mounts the host `/nix/store`
read-only into the containers, so they run straight out of the workspace
build (`packages.default`) and a rebuild needs no in-compose bake; postgres
uses the official `postgres:17.5` image.

Docker alone is enough for that side, with no registry for the KallipAI images:
`scripts/compose-up.sh dev` builds the dev image on the machine that runs the
stack, loads it, and takes `KALLIPAI_IMAGE_TAG` from what it loaded;
`KALLIPAI_DEV_IMAGE` is the reference the compose files then name. Images
loaded by hand under another tag are the mismatch the script reports rather
than a stack that cannot find its image. The observability services come from
upstream registries, pinned by digest, so dev pulls them once.

The tagma is not in either compose file: it runs on the machine where its
agents work, started by whatever that machine provides. Running it with
`KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE` unset leaves it local-only (it logs an
error and keeps serving local agents; see
[Relay bootstrap](#relay-bootstrap)).

Dev is fronted by a Caddy edge proxy that terminates TLS for
`*.kallipai.lan` with an mkcert certificate, so the stack is reachable
cross-machine on the LAN and the browser sees a secure context. The dev
domain comes from `compose/.env` (`compose/.env.example` sets `KALLIPAI_DOMAIN=kallipai.lan`).
One-time host setup is covered in the
[development setup guide](../development/setup.md).

### Production

#### The Platform Host: `docker compose`

The pair `compose.yaml` + `compose.prod.yaml` runs the whole platform on one
host: the four stores, the six application services, the observability stack,
and the edge that terminates TLS and routes each `/v1/<service>` prefix.
Image sources, in order of preference. The seven KallipAI images are built
here by `scripts/compose-up.sh`, which runs `nix build` per image and loads
the result into the docker store, so the default path needs no registry at
all. The eight upstream images are pulled on the first start: four
`postgres:17.5` and the four observability services, each pinned by digest.
On a host with no route to a registry, `docker save` those eight on a machine
that has one and `docker load` them here before the first step.

A host without nix cannot follow the steps as written: `scripts/compose-up.sh`
builds the images before it starts anything, and only `down`, `stop`, `ps` and
`logs` skip the build. Such a host takes the registry route instead: build on a
machine that has nix, push, and start the stack with compose itself. Both
values have to name what was pushed — `KALLIPAI_IMAGE_PREFIX` (it ends in a
slash; the script concatenates) and `KALLIPAI_IMAGE_TAG` (the flake revision,
or `dirty` for a modified checkout) — set in `compose/.env` here, then
`docker compose -f compose/compose.yaml -f compose/compose.prod.yaml up -d`. A private registry
needs `docker login` on both sides.

Two files carry the deployment's values:

- `compose/.env`, which compose reads and interpolates;
  `scripts/compose-env.sh --example prod` prints a starting point for each
  of the seven that belong here, and the table below says where each one
  comes from;
- the service environment file named by `KALLIPAI_ENV_FILE`, whose keys are
  described in `compose/.env.prod.example`.

These are the values a prod render stops on. `compose.yaml` and
`compose.prod.yaml` mark each of them with `${...:?}`, so a value missing
from `compose/.env` stops compose before it starts any container:

| Value | Where it comes from | What it decides |
| --- | --- | --- |
| `KALLIPAI_DOMAIN` | `scripts/compose-env.sh --example prod` | the domain the site serves |
| `KALLIPAI_ENV_FILE` | `scripts/compose-env.sh --example prod` | absolute path of the service environment file |
| `KALLIPAI_INSTANCES_STATE_DIR` | `scripts/compose-env.sh --example prod` | the daemon's runtime directory, mounted into the instances service |
| `KALLIPAI_TLS_DIR` | `scripts/compose-env.sh --example prod` | the certificate pair the edge serves |
| `TELEMETRY_BASE` | `scripts/compose-env.sh --example prod` | where tempo and grafana keep their data |
| `GF_SECURITY_ADMIN_PASSWORD` | `scripts/compose-env.sh --example prod` | Grafana's admin password; prod turns anonymous access off |
| `KALLIPAI_TELEMETRY_INTAKE_TOKEN_FILE` | `scripts/compose-env.sh --example prod` | the path of the bearer token file every OTLP push carries ([Directories and volumes](#directories-and-volumes) lists its mount) |
| `KALLIPAI_STACK_DIR` | `scripts/compose-env.sh --example prod` | the directory the stack renders its observability configs into; the generator prints a starting point, and `scripts/compose-up.sh` exports the path it built |
| `KALLIPAI_IMAGE_TAG` | `scripts/compose-up.sh` | the tag of the images it built or found; the generator does not print it, and the script exports it over any value in `compose/.env` |

The seven in the top group live in `compose/.env`, and the generator prints
a starting value for each. Three of those printed values need replacing: the
password is the `change-me` placeholder, which `scripts/compose-up.sh` refuses
to start with; `KALLIPAI_DOMAIN` is printed as `example.com`, which the same
script refuses; and the instances runtime directory is printed as
`/run/user/<uid>/…`, where `<uid>` is a placeholder the script does not know,
so put your own id there.

The intake token path must name a file that exists and holds the token the
collector expects. The password reaches the grafana service through its own
`service.environment`, which wins over `env_file`, so a value in the service
env file never reaches it.

The last two are the stack's, not yours: `scripts/compose-up.sh` exports both
over whatever `compose/.env` holds, because an env-file tag would name images
that run never loaded and an env-file stack directory would point at configs
that run never rendered.

Operational keys such as listen addresses, the internal token path and the
archeion hop are pinned inside `compose.yaml`; the env file cannot override
them.

The browser-facing keys are not yours to write. CORS origins, the session
cookie domain, the WebAuthn relying party and the OAuth return base follow
`KALLIPAI_DOMAIN` in `compose.prod.yaml`, the way the dev override derives
them from the same value: the app is served at `app.<domain>`, the API at
`api.<domain>`, and the model gateway's forwarding face at `model-gw.<domain>`,
so none of them can disagree with what the edge serves.

A change to a compose file or a Caddyfile is verified by starting the stack once
here: nothing static checks either. Run these in order:

```sh
scripts/compose-up.sh check-env prod   # environment guards and compose's render, no build
scripts/compose-up.sh prod             # build, load, start
scripts/compose-status.sh prod         # every service answers on its health path
```

A start proves the files render and the containers run; it does not prove the
stack is healthy. Read `docker ps` for a service that keeps restarting, and open
the site through the edge once: CORS, the session cookie and the WebAuthn
relying party only show up on a browser path.

Before the first step, have the certificate pair the edge serves
([Certificates](#certificates)) and the directories the stack binds
([Directories and volumes](#directories-and-volumes)) on the host. The site is
not one of them: it travels inside the edge image
([The site the edge serves](#the-site-the-edge-serves)).

##### Certificates

The host keeps issuing and renewing the certificates; nothing in the stack
runs ACME. `KALLIPAI_TLS_DIR` holds `fullchain.pem` and `privkey.pem` and is
mounted read-only at `/certs`, which is where `KALLIPAI_TLS_CERT` and
`KALLIPAI_TLS_KEY` point.

The pair has to cover three names, because the edge serves three hosts:
`app.<domain>`, `api.<domain>` and `model-gw.<domain>`. A wildcard
`*.<domain>` covers all three and is what `compose.prod.yaml` expects; a
certificate listing names has to carry all three as SANs. A pair that names
only the first two serves the first two hosts: a client reaching
`model-gw.<domain>` gets a name mismatch, so reissue before that host goes
live.

Caddy loads the pair when it starts and holds it until it reloads, so a
renewed certificate reaches the browser only after:

```sh
docker compose -f compose/compose.yaml -f compose/compose.prod.yaml exec caddy \
  caddy reload --config /etc/caddy/Caddyfile
```

Run that from the renewal hook, after the new files are in place. A hook that
served this host before usually reloads its own caddy unit, which step three
below stops and disables: point the hook here instead, or it keeps asking a
process that no longer holds the port. A reload that fails (an invalid
Caddyfile, a missing file) prints to the caller and leaves caddy serving the
configuration and certificates it already had; `docker compose logs caddy`
carries the same message. Renewal must not depend on binding port 80 or 443 on
the host: those belong to the container, so use a DNS challenge or a webroot
the edge serves.

##### Directories and volumes

Directories the host provides:

| Variable                               | Mounted at                   | Contents                                                    |
| -------------------------------------- | ---------------------------- | ----------------------------------------------------------- |
| `KALLIPAI_TLS_DIR`                     | `/certs`                     | `fullchain.pem`, `privkey.pem`                              |
| `KALLIPAI_INSTANCES_STATE_DIR`         | `/state`                     | the directory holding the daemon's control socket           |
| `TELEMETRY_BASE`                       | tempo, grafana               | their data, written by the containers                       |
| `KALLIPAI_TELEMETRY_INTAKE_TOKEN_FILE` | `/etc/kallipai/intake-token` | the bearer token every OTLP push carries, mounted read-only |
| `KALLIPAI_STACK_DIR`                   | config files                 | the observability configs, produced by the script           |

`KALLIPAI_INSTANCES_STATE_DIR` is the daemon's runtime directory, not a
directory the stack creates: point it at the directory the daemon listens in
(`$XDG_RUNTIME_DIR/kallipai/daemon` on a desktop host) and the instances
service finds its socket at `/state/control.sock`. The service resolves that
socket before it binds, so a missing one ends the process and
`restart: unless-stopped` starts it again: the container sits in
`Restarting`, never `Up`. A socket that disappears after the start is the
other shape: the service stays up and answers `/health` and `/list` with
`503`. A directory docker creates empty gives the first shape, so check the
mount before the first start.

The daemon is the tagma host's, and this round leaves the tagma where it
runs today. When that is another machine, leave `instances` out of the
step-one list: the socket does not cross hosts, and the instance list has
nothing to read until the two share one.

Volumes, by name: `archeion_pgdata`, `lesche_pgdata`, `gateway_pgdata`,
`files_pgdata`, `kallipai_files_blobs`, `polis_internal`, `prometheus_data`.
The project name `kallipai-archeion` decides their names, and `polis_internal`
is where the archeion writes the internal token its neighbours read. A host
that has run this stack before already holds five of them under these names,
and this pair adds `gateway_pgdata` and `prometheus_data`. On a fresh host,
compose creates all seven on first use.

##### The site the edge serves

The bundle lives in the edge image (`packages.kallipai-caddy-web-image`):
`nix/packages/docker-images/caddy-web.nix` copies `packages.kallipai-web-dist`
into `/srv/web` beside the entries and the body they share, and the image's
caddy serves it. Nothing is copied onto the host. The runtime config is not a
file either: the edge answers `/config.js` with a response built from its own
environment, so `KALLIPAI_DOMAIN` and `KALLIPAI_API_BASE` decide what the
browser reads, and changing the domain restarts this container instead of
rebuilding the site. The payload states `instancesServiceEnabled: true` as a
constant: this stack always runs its instances service, and the SPA reads an
unset key the same way. The api host is served at `KALLIPAI_API_BASE` itself,
scheme and port included, so the address the browser is given and the one the
edge answers on are one value.

The model gateway keeps a host of its own, `KALLIPAI_MODEL_GW_SITE`, for its
forwarding face. The gateway's forwarding routes carry no `/v1` prefix, while a
client base ends in `/v1`, so that host strips the prefix once
(`handle_path /v1/*`) and forwards the bare path; a fallback `reverse_proxy` in
the same block keeps the unprefixed form working. The base the gateway
publishes is therefore `https://model-gw.<domain>/v1`. The management face is
not on this host: it stays on the api host, under `/v1/model-gateway/*`.

The image carries three entries and one body (`compose/site.caddy`):

| Entry              | App host answers with            | TLS  |
| ------------------ | -------------------------------- | ---- |
| `Caddyfile`        | the bundle in the image          | yes  |
| `Caddyfile.plain`  | the bundle in the image          | no   |
| `Caddyfile.vite`   | `127.0.0.1:5173`, the vite server | yes  |

Each entry declares its own site blocks: the app host, the api host and the
model gateway's forwarding host. They carry the body's snippets and differ only
in the answers and the addresses they name: that is the whole difference between
them. The edge container picks one with its `command:`; the production override
leaves the image's own default (`Caddyfile`), and the dev override names it through
`KALLIPAI_EDGE_ENTRY`. `Caddyfile.plain` exists as a separate entry because
caddy refuses an `http://` address that also configures TLS ("server is HTTP,
but attempts to configure TLS"), so an https-less stack has to reach the body
through a file without a `tls` line.

All three import the one body, which pins `h1` and `h2`: caddy otherwise
advertises h3 through `alt-svc`, and Firefox upgrades the cross-subdomain SSE
stream (`/v1/lesche/me/events`) to it and hangs the long-lived response.

Upgrading the site is a new image tag:

```sh
scripts/build-and-load-images.sh caddy-web
docker compose -f compose/compose.yaml -f compose/compose.prod.yaml up -d caddy
```

The build loads the image under the flake revision (or `dirty`) and also
under `:latest`, and compose names whatever `KALLIPAI_IMAGE_TAG` holds.
Rolling back means setting that value to the previous batch's tag and running
the same `up -d` again, which is why the previous image stays loaded
(`docker image ls kallipai-caddy-web`). A store path printed by `nix build` is
not the durable artifact here; the docker store is.

Check: `curl -sS https://app.<domain>/config.js` shows the domain and the API
base this deployment actually serves, so the browser does not fall back to
deriving them from its own location.

##### The handover (once)

The host edge and the stack's caddy cannot both own ports 80 and 443, so
caddy starts only after the host unit has let go. Everything else comes up
first, which keeps the handover to the seconds between steps three and four.

Steps one to four change the host over; the rest of this subsection is what
keeps the stack running afterwards. The handover is not repeated on a
restart: the compose project owns the ports from then on.

Step one takes the host over rather than starting clean. The stack that runs
there already uses the project name `kallipai-archeion` and the same volume
names, so compose adopts the running containers and the data already in those
volumes; the archeion migrates that schema at boot. The platform is down
from the moment the old containers stop until the new ones answer, and step
one is the longest part of that window.

1. Start every service but the edge:

   ```sh
   scripts/compose-up.sh prod up -d archeion lesche files keryx gateway \
     instances otelcol tempo grafana prometheus archeion-postgres \
     lesche-postgres gateway-postgres files-postgres
   ```

   The service env file carries the old deployment's values: `KALLIPAI_ENV_FILE`,
   named in `compose/.env`, points at the file the previous release used, and
   that file's `POSTGRES_PASSWORD` and four `*_DATABASE_URL` entries are the
   ones the existing volumes were initialised with. The example's `change-me`
   values do not authenticate against them; `scripts/compose-up.sh` refuses to
   start while one of them is still in the file.

   Check in two ends. `docker compose -f compose/compose.yaml -f compose/compose.prod.yaml ps`
   shows every service `running`, and that is only a sample: a service that
   exits at boot is running at almost every instant, the gap being the restart
   itself. So compare restart counts across a twenty-second window, from the
   repository root:

   ```sh
   counts() {
     for id in $(docker compose -f compose/compose.yaml -f compose/compose.prod.yaml ps -q -a); do
       printf '%s %s\n' "$id" "$(docker inspect -f '{{.RestartCount}}' "$id")"
     done
   }
   counts > /tmp/a; sleep 20; counts > /tmp/b; diff /tmp/a /tmp/b
   ```

   No difference is the verdict: every container held for the window.

2. Probe the services: `scripts/compose-status.sh prod`.

   Check: five services report `ok`, and instances reports `up` with a `401`.
   Every instances route sits behind the platform token, so without the
   credential a probe can prove only that much: the listener is up and the
   guard is armed. instances also refuses a Host it was not configured for, and
   from inside the platform network it answers to its service name, so that
   name sits in `KALLIPAI_INSTANCES_ALLOWED_HOSTS` beside the public one. The
   probe runs on the platform network and never crosses the edge: the edge
   image carries busybox so that the edge container, or a one-shot container of
   the same image, has a shell and `wget` to ask with.

   A deployment whose allowlist predates the instances name answers the probe
   with `403`, and the run fails on the instances line. The list is
   `KALLIPAI_INSTANCES_ALLOWED_HOSTS` in the service env file
   (`compose/.env.prod.example` shows the shape): add `instances` beside the
   public name. The list reaches the process through the compose-level
   `env_file`, which compose reads once, when it creates the container, so
   recreate the service after the edit — `scripts/compose-up.sh prod -- up -d
   instances`, with `--force-recreate` when compose keeps the container — and
   probe again. `docker compose restart` keeps the environment the container
   was created with.

   The probe proves less than it looks like. `/healthz` answers `ok`
   unconditionally, so a wrong database URL still reads `ok`;
   `docker compose -f compose/compose.yaml -f compose/compose.prod.yaml logs archeion` shows
   `connected to Postgres` once the store answers, and that is the store's
   verdict.

3. Stop and disable the unit that terminates TLS today. The port names the
   process, the process names the unit, and the unit is what gets disabled:

   ```sh
   ssh-host# ss -ltnp '( sport = :443 )'   # the pid is in users:(("caddy",pid=1234,...))
   ssh-host# systemctl status 1234         # the unit is in the first line
   ssh-host# systemctl disable --now <unit>
   ```

   Check: `systemctl is-enabled <unit>` prints `disabled`, and
   `ss -ltnp '( sport = :443 )'` lists no listener. Disabling a process name
   instead of the unit it belongs to stops the wrong thing: read the unit off
   the process, do not assume it.

4. Start the edge: `scripts/compose-up.sh prod up -d caddy`.

   Check: `curl -sSI https://app.<domain>/` returns `200`, and
   `docker compose -f compose/compose.yaml -f compose/compose.prod.yaml ps caddy` is `Up`.

Only steps three and four reverse on their own: stop the caddy container,
then `systemctl enable --now <unit>`; the host has to own the ports before
its unit can serve, so the container stops first. Confirm with
`ss -ltnp '( sport = :443 )'` that the host process listens again.

Step one does not reverse. The new containers write to the same volumes and
the archeion has already migrated the schema, so the old stack would come
back onto data it does not know. Take a backup before step one and keep it:
`pg_dump` of each store from its running postgres container (archeion,
lesche, files), plus a tar of the `kallipai_files_blobs` volume. Going back
then means restoring those into the volumes and starting the old stack
against them.

##### Restarts

The stack has no systemd unit of its own. Each service carries
`restart: unless-stopped`, so the containers return with the docker daemon
after a reboot; a host that reboots without docker enabled comes back with
nothing running. Verify once with a reboot on the rehearsal host:
`docker compose -f compose/compose.yaml -f compose/compose.prod.yaml ps` must show every
container `Up` without a manual start.

##### Upgrades

A new version is a new checkout and one more start. Move the checkout to the
revision you want, then run `scripts/compose-up.sh prod`: it builds that
revision's images, loads them, and starts the stack on the tag it loaded. A
`KALLIPAI_IMAGE_TAG` in `compose/.env` does not decide that — the script
exports its own over it, so the file cannot point the stack at images the run
never loaded. The stack keeps its volumes.

On a host without nix, the registry route above applies: push the new batch
from a machine that has nix, then set `KALLIPAI_IMAGE_PREFIX` and
`KALLIPAI_IMAGE_TAG` in `compose/.env` and run
`docker compose -f compose/compose.yaml -f compose/compose.prod.yaml up -d`.
There the tag in the file decides, and it has to name a batch that was pushed.

Each service migrates its own store at boot, so stop the stack first when the
release notes name a maintenance window. When they name an index build,
[Upgrades Run Migrations](index.md) says why the build wants a quiet one and
how much of the table makes it matter.

#### The Tagma Host

The tagma runs from `packages.kallipai-tagma-image` on the machine where its
agents work, and no composition in this repo starts it: the image carries the
binaries (`kallipai-tagma` is the entry point) and the caller supplies the
command, the env and the mounts. The relay connector talks to the deployed
platform over the public internet through one origin (`KALLIPAI_POLIS_URL`,
e.g. `https://api.kallipai.com`): enrollment happens once on first boot
against that origin, and the relay connection is held through it afterwards.

```sh
docker run -d --name kallipai-tagma --restart unless-stopped \
  -p 3000:3000 \
  -v kallipai_tagma_data:/var/lib/kallipai/tagmata/main \
  -v "$PWD/workspace":/workspace \
  -e KALLIPAI_POLIS_URL=https://api.<your-domain> \
  -e KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE=<code> \
  kallipai-tagma:<tag> kallipai-tagma
docker logs -f kallipai-tagma
```

The volume holds the agent state, logs, skills and the tagma's credentials, so
it is the piece worth backing up.

Secure the tagma's published `3000` port (the operator API): do not expose
it on a public host without a firewall / TLS reverse proxy in front.

#### The Platform Host: `scripts/compose-up.sh prod`

Brings up the platform's services (the archeion identity control plane, the
lesche relay data plane, the keryx tagma-link relay, and the files service),
plus one postgres store per stateful service (`postgres:17.5`), co-located on
one host. **None of the services is published**: all sit behind your
TLS-terminating edge proxy, which path-routes the `api.<your-domain>` host by
service and gives the model gateway's forwarding face a second host,
`model-gw.<your-domain>`; `reverse_proxy` sets `X-Forwarded-For` on its own, and
you configure `KALLIPAI_ARCHEION_TRUSTED_PROXIES` to the proxy's CIDR. The edge
must not route `/v1/archeion/internal` or anything under it publicly (that
is the archeion's `/internal` and `/internal/*` once the prefix is
stripped): the relay, the tagma-link relay and the files service reach it
over the private compose network.
Secret-bearing env and the postgres credentials come from `compose/.env`;
operational env is pinned in `service.environment`, which overrides `env_file`.

The path shape follows the clients: every client base is `{origin}/v1/{service}`
(`crates/kallipai-common/src/polis.rs`), so the edge strips the `/v1/<service>`
segment before the service — the services themselves serve bare paths (the
archeion's `/healthz`, `/me/*` and `/admin/*` carry no `/v1`). The `files`
service also answers its bare root (`/v1/files`), which a
`handle_path /v1/files/*` rule would miss, so it takes a named matcher
on both forms and strips with `uri strip_prefix /v1/files`. The route
table lives in `compose/site.caddy`, the body all three entries import; the
seven upstream addresses come from the environment, `compose.dev.yaml` pointing them
at loopback and `compose.prod.yaml` at the service names.
An unlisted `/v1/*` prefix is a real 404:

```caddy
api.<your-domain> {
  @internal path /v1/archeion/internal /v1/archeion/internal/*
  handle @internal {
    respond 404
  }
  handle_path /v1/archeion/* {
    reverse_proxy archeion:7100
  }
  handle_path /v1/lesche/* {
    # SSE flushes by content type; the non-SSE fallback lives in the full configs
    reverse_proxy lesche:7200
  }
  handle_path /v1/keryx/* {
    reverse_proxy keryx:7210
  }
  @files path /v1/files /v1/files/*
  handle @files {
    uri strip_prefix /v1/files
    reverse_proxy files:7400
  }
  # An unlisted /v1/* prefix is a real 404.
  respond 404
}
```

The forwarding face takes its own host. Its routes carry no `/v1` prefix, so
the host strips the prefix once and forwards the rest; the fallback serves a
client that points at the bare origin, and both flush immediately because a
streaming completion holds one response open for minutes:

```caddy
model-gw.<your-domain> {
  handle_path /v1/* {
    reverse_proxy gateway:7501 {
      flush_interval -1
    }
  }
  reverse_proxy gateway:7501 {
    flush_interval -1
  }
}
```

`KALLIPAI_MODEL_GATEWAY_PUBLIC_BASE_URL` is then
`https://model-gw.<your-domain>/v1`. The `/v1/model-gateway/*` segment on the
api host is the management face, which has no forwarding route. The stack's own
edge writes that block from the `(model-gw-routes)` snippet in
`compose/site.caddy`. `compose.prod.yaml` derives the base from
`KALLIPAI_DOMAIN`, so it moves with the site. Set it through a compose-level
variable (`compose/.env`, or exported before `scripts/compose-up.sh`) when the
forwarding face answers on another host: the service environment file cannot
set it, because a value in `service.environment` wins over `env_file`. A base
assembled by hand as `{base}/{path}` must carry no trailing slash: the data
plane matches exact paths, so `…/v1/` there becomes `//chat/completions` and
answers 404. The in-house clients normalise the join themselves, so the slash
only bites a script that concatenates the two parts.

```sh
scripts/compose-up.sh prod up -d
docker compose -f compose/compose.yaml -f compose/compose.prod.yaml logs -f
```

Each platform service writes its events to stdout, and the compose `json-file`
driver keeps them for the operator: 50 MB per file, five files per service,
pinned at the `x-logging` anchor in `compose/compose.yaml`. Adding a service
name to the `logs` call above narrows the output to that one service.

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
  archeionPackage = inputs.self.packages.x86_64-linux.kallipai-archeion;
  leschePackage = inputs.self.packages.x86_64-linux.kallipai-lesche;
  keryxPackage = inputs.self.packages.x86_64-linux.kallipai-keryx;
  filesPackage = inputs.self.packages.x86_64-linux.kallipai-files;
  instancesPackage = inputs.self.packages.x86_64-linux.kallipai-instances;
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
deployment-independent. The dev form of the same site is the host's vite
server, which the dev edge reaches through `Caddyfile.vite`.

A deployment that serves the web app but runs no instance host (no
polis instances service backed by a local daemon) declares that in the
same payload, unless its browser reaches an instances API elsewhere:
`runtimeConfig.instancesServiceEnabled = false`. The `/tagmata` page
then neither probes the service nor renders its process half, instead
of reporting a service the deployment does not serve. The NixOS module
derives the value from the deployment's own switches unless the
deployment sets the key itself: it is true only where polis and the
daemon both run, false where only one of the two runs, and left unset
where neither does. A deployment that runs neither and whose browser
reaches no instances API sets the key to false itself. A deployment
outside NixOS takes the other route and hand-edits `config.js`, setting
`instancesServiceEnabled: false` on the same `window.KALLIPAI_CONFIG`
object it already edits.

### Relay Bootstrap

The tagma enrolls with the platform on its **first** boot, using a
single-use enrollment code minted via the platform dashboard after a user
signs up (`KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE`). A machine holding a
personal access token can mint the code and pass it along in one step:
`kallipctl spawn <slug> <workspace> --polis <origin>` mints a code and
starts the instance with it set. The issued tagma token is
persisted inside the `data` volume and reused after that; leave the code
unset on subsequent boots. If enrollment cannot complete, the tagma logs an
error, keeps serving local agents, and comes back to it on a later boot
(`restart: unless-stopped`): check `docker logs kallipai-tagma`.

### Run-Time Privileges

The tagma runs agent shells inside a sandbox (kernel-level filtering plus an
isolated mount namespace), applied **fail-closed**: if any step is blocked,
the spawned shell aborts. Whatever starts the tagma container grants what this
needs on that container only. The integration-test composition
(`compose.test.yaml`) runs the suites under the same privileges, on its own
service:

- `service.capabilities.SYS_ADMIN = true` (→ `cap_add: [SYS_ADMIN]`)
- `out.service.security_opt = [ "seccomp=unconfined" ]`

The platform services and both postgres services need no special
privileges.

Docker's default shape is rootful: the container's uid 0 is the host's uid
0, exactly what the sandbox guard refuses, since every spawned instance
would be a host-root process. Prefer a rootless or userns-remapped runtime.
Where that is not an option, set
`KALLIPAI_TAGMA_ACCEPT_UNSAFE_RUN_AS_ROOT=1` (exact value) in the tagma
service's environment and accept the per-boot warning; anything else:
unset, empty, `0`, keeps the refusal.

The grant lives on the compose `service.environment` line, not in the env
file: the tagma services also load `compose/.env` (via `env_file`), but an
explicit `service.environment` entry wins, so the consent is granted and
revoked by editing the compose file, never the env file.

### Volumes and Workspaces

In dev and the prod-tagma composition, tagma data and the agent workspace
are **docker named volumes**: no host directories are created and the
project tree stays clean. Shared skills live inside the `kallipai_tagma_data`
volume's `skills/` subdir. The platform host and each postgres service add
their own volumes in the compositions that run them. The test composition
mounts none (its scratch tree is an ephemeral `/testdata` tmpfs).

- `kallipai_tagma_data` named volume → `/var/lib/kallipai/tagmata/main`: agent state, logs, skills, and the tagma's credentials (persistent; it outlives the container, and `docker volume rm` is what removes it).
- `workspace` named volume → `/workspace`: the agent workspace root.
- `archeion_pgdata` named volume → `/var/lib/postgresql/data`: the identity control plane's Postgres store (dev + the prod-platform composition).
- `lesche_pgdata` named volume → `/var/lib/postgresql/data`: the relay data plane's Postgres chat store (dev + the prod-platform composition).
- `files_pgdata` named volume → `/var/lib/postgresql/data`: the files service's Postgres store (dev + the prod-platform composition).
- `kallipai_files_blobs` named volume → `/var/lib/kallipai/files/blobs`: the files service's content-addressed blob store (dev + the prod-platform composition).

The tagma's data and workspace are mounts its starter chooses, so bind-mounting
them to host paths is a property of the `docker run` above rather than of any
file here: point `-v` at the directories that should outlive the container.

`workspace_root` passed to the tagma API is always resolved as an
in-container path (default `/workspace`); a host bind does not change what
the tagma sees.

Each agent needs a `workspace_root` that exists in the container and is
**disjoint** from `/var/lib/kallipai/tagmata/main`. Pass
`workspace_root: /workspace` when creating an agent via the tagma API; the
tagma rejects a workspace that contains or is contained by the data dir.

### Environment

The compose files hardcode per-service defaults, and `compose/.env` supplies
the rest: tokens and the prod-platform deploy secrets. Compose precedence
means `service.environment` wins over
`env_file`, so values the dev compose hardcodes are not overridable
via `compose/.env` in that mode. The full variable tables live in the
[environment-variables reference](../reference/environment-variables/index.md).

#### Model Configuration

Model configuration lives in a file. The tagma services set
`XDG_CONFIG_HOME=/var/lib/.config` and mount the
`kallipai_tagma_config` volume at the instance config root it resolves to
(`/var/lib/.config/kallipai/tagmata/<slug>`), so the `profiles.toml` the
Profiles page writes — or one copied in with `docker cp` — survives
container recreation. The variable applies to every XDG consumer in the
container, not just the tagma: a `git` or `gh` run inside an agent
shell reads its tool configuration from that same config home. The file's
shape is documented in
[model configuration methods](../configuration/tagma/methods.md); a
deployment on the model-gateway source takes its profiles from there
instead.

### Observability

Grafana listens on `3000` inside its container. The dev stack publishes it at
`127.0.0.1:3001` and opens as an anonymous Viewer. The prod stack publishes
otelcol's `127.0.0.1:4318` and `127.0.0.1:13133` and nothing else for
observability: Grafana and Prometheus get no host port, and the edge has no
route to them. To open the prod UI, put `ports: ["127.0.0.1:3002:3000"]` on
the `grafana` service in `compose/compose.prod.yaml` and run
`scripts/compose-up.sh prod -- up -d grafana`, which recreates the container;
prod also turns anonymous access off, so sign in as `admin` with the password
Grafana was first started with (`GF_SECURITY_ADMIN_PASSWORD`).

Grafana starts with two provisioned data sources, both reached by service
name: Tempo, the default, at `http://tempo:3200`, and Prometheus at
`http://prometheus:9090`. Prometheus publishes no host port in either stack,
because its write receiver takes no credential; it scrapes `gateway:7500` and
takes otelcol's remote writes, so metrics are read through Grafana rather
than from a browser pointed at the store. The data source list and
`grafana.ini` are both rendered from `nix/obs-stack.nix`; `prometheus.yaml`
comes from `nix/platform-texts.nix`. `nix/packages/stack.nix` installs all
five texts into the stack directory that `scripts/compose-up.sh` rebuilds on
its next start. Traces, retention and the pruner are covered in
[observability](../development/observability.md).

### Plain Docker

Every image can be built and loaded by hand. The tagma runs from
`kallipai-tagma-image`:

```sh
nix build .#kallipai-tagma-image
docker load < result
# The tag is the built git version, not "latest" — take it from the
# "Loaded image" line docker load just printed:
docker images | grep kallipai-tagma
docker run --rm \
  --security-opt seccomp=unconfined --cap-add SYS_ADMIN \
  -p 3000:3000 \
  -v kallipai-tagma_data:/var/lib/kallipai/tagmata/main \
  -v kallipai-tagma_config:/var/lib/.config/kallipai/tagmata/main \
  -v kallipai-tagma_workspace:/workspace \
  -e HOME=/var/lib -e KALLIPAI_TAGMA_SLUG=main \
  -e XDG_DATA_HOME=/var/lib -e XDG_STATE_HOME=/var/lib \
  -e XDG_CONFIG_HOME=/var/lib/.config \
  -e KALLIPAI_WORKSPACE_ROOT=/workspace \
  -e KALLIPAI_TAGMA_ADDR=0.0.0.0:3000 \
  -e KALLIPAI_TAGMA_ACCEPT_UNSAFE_RUN_AS_ROOT=1 \
  kallipai-tagma:<gitVersion> kallipai-tagma
```

(The image has no default `Cmd`: pass the binary name `kallipai-tagma`
explicitly. The `-e` lines mirror the compose `tagma` service: the slug and
the XDG homes are what make the mounted volumes — the config one included —
land where the tagma looks for them, and the root escape is the documented
flag for a rootful container with no uid mapping.)

The platform's control plane runs from `kallipai-archeion-image` (behind your
own TLS reverse proxy and a postgres):

```sh
nix build .#kallipai-archeion-image
docker load < result
# The tag is the built git version, not "latest" — take it from the
# "Loaded image" line docker load just printed:
docker images | grep kallipai-archeion
docker run --rm \
  -e KALLIPAI_ARCHEION_DATABASE_URL=postgres://kallipai:...@postgres:5432/kallipai \
  -e KALLIPAI_ARCHEION_WEBAUTHN_RP_ID=<your-domain> \
  -e KALLIPAI_ARCHEION_WEBAUTHN_RP_ORIGIN=https://app.<your-domain> \
  -e KALLIPAI_ARCHEION_CORS_ORIGINS=https://app.<your-domain> \
  kallipai-archeion:<gitVersion>
```

The WebAuthn RP id is the registrable domain the app face sits under; the
origin and the CORS entry both point at that app subdomain.
The id cannot change later without invalidating every bound passkey.

Then create an agent via the tagma API with `workspace_root: /workspace`.

#### All Eight Images At Once

`scripts/build-and-load-images.sh` builds every image the compose stack
runs, plus the tagma, and loads their tarballs into the local daemon in
one run. It calls `nix build --no-link --print-out-paths` per attribute, so
no `result` symlink is created or overwritten, and it stops at the first
failure:

```sh
scripts/build-and-load-images.sh            # all eight
scripts/build-and-load-images.sh tagma      # one image
```

The last thing it prints is the tag the batch carries, shaped to be copied
straight into a `.env` file:

```text
KALLIPAI_IMAGE_TAG=<tag>
```

The tag is the one the flake evaluation produced: a clean worktree gives the
short commit hash, uncommitted changes give `dirty`. The script builds each
image in its own evaluation, then compares the tags as they load, so the
promise is one tag per batch. A `dirty` batch proves nothing about the tree:
two builds of different dirty trees are both `dirty`, and the check cannot
tell them apart.

Each image also gets `:latest`, so `KALLIPAI_IMAGE_TAG=latest` names the batch
that was loaded last without copying the batch tag around. Pinning the batch
tag in `.env` keeps a later build from moving a running stack; pasting an
older batch tag rolls back to it.

The eight attributes are `kallipai-archeion-image`, `kallipai-lesche-image`,
`kallipai-keryx-image`, `kallipai-files-image`,
`kallipai-model-gateway-image`, `kallipai-instances-image`,
`kallipai-caddy-web-image` and `kallipai-tagma-image`. The selector is the
middle word (`archeion`, `lesche`, `keryx`, `files`, `model-gateway`,
`instances`, `caddy-web`, `tagma`); `--help` prints the same list. The
observability services are not built here: the collector, tempo, Grafana and
Prometheus come from upstream registries, pinned by digest in the two
override files. The collector is the stack's only OTLP door, on `4318` behind a
bearer token read from `KALLIPAI_TELEMETRY_INTAKE_TOKEN_FILE`; it forwards
traces to tempo and metrics to Prometheus over the remote-write protocol. The
edge image is the one the production stack serves the
browser from: it carries the site, so the two move together.
