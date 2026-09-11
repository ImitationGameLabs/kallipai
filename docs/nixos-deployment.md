# NixOS deployment

This guide walks through deploying the kallipai platform form on a NixOS
host: the daemon as a system service, the four polis services behind the
host's reverse proxy, and the web app. The container-based deployments
(arion compose) are covered in [container.md](reference/container.md);
this guide is the step-by-step bring-up for the NixOS module form. The
module itself is `nix/nixos-modules.nix`, and its option descriptions are
the authoritative reference for everything this guide summarizes.

## Prerequisites

- A NixOS host with flakes enabled
  (`nix.settings.experimental-features = [ "nix-command" "flakes" ]`).
- For the proxied shape: DNS records pointing the subdomains (`archeion.`,
  `lesche.`, `files.`, `instances.`, and optionally `web.`) at the host.
  On a public domain with ports 80 and 443 reachable, Caddy obtains
  certificates automatically; on a private domain like the `kallipai.lan`
  example below, ACME cannot issue — use `tls internal` in your site
  (see the TLS section).
- Root access (to rebuild the host and to read the generated admin
  token at first login).

## Import the module

The module ships as the flake output `nixosModules.kallipai` (also
exported as `nixosModules.default`). Add the repository as a flake input
and import the module into your host:

```nix
{
  inputs.kallipai.url = "github:ImitationGameLabs/kallipai";

  outputs = { nixpkgs, ... }@inputs: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        inputs.kallipai.nixosModules.kallipai
        ./configuration.nix
      ];
    };
  };
}
```

## The internal token (self-managed)

The polis services authenticate to each other with one shared secret,
and the archeion owns its lifecycle: on first boot it generates the
secret into its state directory
(`/var/lib/kallipai/archeion/internal-token`, mode 0640, readable by
the `kallipai-polis` group the module creates), and on every later
boot it reads the existing value — a token that exists is never
rewritten. The lesche, files, and instances services read the same
file, so all four agree on one value for the lifetime of the
deployment.

Secret state is filed by lifetime: `/etc` holds administrator-owned
static configuration (a pinned admin token), `/run` holds volatile
runtime state (the auto-generated admin token, reset on every service
restart), and `/var/lib` holds service-owned persistent state (the
internal token, stable across restarts).

To rotate the internal token: stop the four polis services, delete the
file, start the archeion (a fresh value is generated), then start the
other three. The group restart keeps every service on the same
generation.

## Minimal configuration

A minimal full-platform configuration — the daemon, the four polis
services, and the web site behind Caddy. One module function is the
whole `configuration.nix`: copy it once and the platform is up.
`services.kallipai.domain` is the knob the rest derives from: the
services' CORS allow-lists, the session cookie domain, webauthn, and
the baked runtime config all follow it, and `tls = false` keeps every
derived origin on `http` to match the plain-http serving. The web site
serves `config.services.kallipai.web.distWithRuntimeConfig` — the
bundle as-is while `runtimeConfig` is empty, or the bundle with a
runtime config baked into `config.js` once keys are set. The site
addresses are plain `http://` on purpose: ACME cannot issue for a
`.lan` domain, so plain http runs with zero browser setup — the HTTPS
section below shows the two upgrade paths.
One caveat: a plain-http page on a real domain is not a secure
context, so browsers refuse passkey ceremonies there —
`localhost`-based dev keeps them, and the https upgrades below restore
them.

```nix
{ config, ... }:
let
  domain = "kallipai.lan";
  # The lesche streams event payloads, so its proxy disables response
  # buffering; the other backends proxy bare.
  backendExtra = {
    lesche = ''
      reverse_proxy 127.0.0.1:${toString config.services.kallipai.polis.ports.lesche} {
        flush_interval -1
      }
    '';
  };
  backendVhost = svc: {
    name = "http://${svc}.${domain}";
    value.extraConfig = backendExtra.${svc} or ''
      reverse_proxy 127.0.0.1:${toString config.services.kallipai.polis.ports.${svc}}
    '';
  };
in
{
  services.kallipai = {
    inherit domain;
    tls = false; # plain http: ACME cannot issue for a .lan domain
    daemon.enable = true;
    polis.enable = true;
    web.enable = true;
  };
  services.caddy = {
    enable = true;
    virtualHosts = {
      "http://web.${domain}".extraConfig = ''
        root * ${config.services.kallipai.web.distWithRuntimeConfig}
        try_files {path} /index.html
        file_server
      '';
    } // builtins.listToAttrs (map backendVhost [
      "archeion"
      "lesche"
      "files"
      "instances"
    ]);
  };
}
```

The module provisions its own PostgreSQL — one database per stateful
service (archeion, lesche, files), peer-authenticated over the unix
socket — so no database setup is needed. The daemon supervises tagma
instances over a local control socket and is consumed by `kallipctl`
and the instances proxy. Enabling the daemon also puts every platform
command on the system PATH, so no separate CLI install step exists.

To drive the daemon with `kallipctl`, admit the user to the
daemon's socket group — the control socket lives at
`/run/kallipai/daemon.sock` and is gated to the `kallipai-daemon`
group:

```nix
users.users.alice.extraGroups = [ "kallipai-daemon" ];
```

The package options default to this flake's full workspace build;
setting one explicitly pins that service's own binary. With `adminTokenFile`
unset, the archeion mints a fresh admin
token on every start into its runtime directory
(`/run/kallipai/archeion/admin-token.env`, mode 0600) and logs only the
path — the value never appears in the journal. Read it for the first
login with:

```sh
sudo cat /run/kallipai/archeion/admin-token.env
```

That token is a short-lived bootstrap credential: rewritten on every
restart, and sessions minted with it live in the database and survive
token rotation. A permanent deployment pins the
file instead (the `adminTokenFile` option description covers the file
format and the OAuth client secrets it can carry) — pin for a stable
token, leave unset to accept a short-lived one.

## Users, launch identities, and the real-root guard

The module's daemon unit runs as `root` — a system service that reads
every declared user's passwd entry — while the declared `tagmaUsers`
are the only launch identities instances ever run as. A root daemon
refuses an inferred in-place launch (the error asks for `--user`), so
every `adopt` and `start` on NixOS names one of the declared users
explicitly; nothing the platform hosts runs as the host's real root,
and a tagma started directly as real root refuses to boot outright
(the escape hatch is `KALLIP_TAGMA_ACCEPT_UNSAFE_RUN_AS_ROOT=1`, not
meant for this deployment form).

The declared users are provisioned with a home directory and linger:
`/home/<user>` holds the instance's XDG config, data, and state
trees, and logind pre-creates `/run/user/<uid>` at boot — the spawned
instance's `XDG_RUNTIME_DIR`, with no per-instance setup. Without
linger the runtime directory is simply absent: an instance still
binds its TCP port and never reads `XDG_RUNTIME_DIR`, so nothing
else breaks. Service-owned persistent state (the polis services'
stores and the internal token) stays under `/var/lib/kallipai`,
separate from the per-user homes — the same lifetime split the
internal-token section describes.

## The reverse proxy (your edge)

The Minimal configuration block above is the complete edge: one Caddy
site per public name reverse-proxying to the localhost listeners, plus
one static site for the web app. What each piece does:

- The four polis subdomains proxy to the localhost ports listed in the
  Ports section below. The lesche route flushes immediately
  (`flush_interval -1`) so the event stream does not buffer behind the
  proxy; the other three are plain request/response.
- The web block serves the root from
  `config.services.kallipai.web.distWithRuntimeConfig` — the bundle
  as-is when `runtimeConfig` is empty, or the bundle with your runtime
  config baked into `config.js` — with an `index.html` fallback so
  client-side routes resolve on hard reload. Set keys under
  `services.kallipai.web.runtimeConfig` to override or add values (for
  example `offlineLogin = false` on a cloud-facing deployment, or
  `domain` when the app cannot derive it); unset keys fall back to the
  app-side derivation, and an unknown key fails evaluation, so typos
  surface at build time.
Two upgrades from the plain-http block above:
- Keep `kallipai.lan` and want https: set `services.kallipai.tls = true`
  (or drop the line — it is the default) so the derived origins speak
  https, drop the `http://` prefixes from the site-block addresses, and
  add `tls internal` to each block (internal CA certificates — see the
  HTTPS section below for trusting its root).
- Move to a public domain: change the `domain` binding to the public
  name, drop the `http://` prefixes the same way, and set
  `services.caddy.email`; with ports 80 and 443 reachable, Caddy
  obtains real certificates automatically.

## Ports

The four listeners bind localhost on 7100 (archeion), 7200 (lesche),
7400 (files), and 7300 (instances). Override any of them under
`services.kallipai.polis.ports.<service>` (1024-65535; the four values
must be distinct — the module fails evaluation otherwise). The proxy
blocks above read these ports from the module, so a changed port
re-targets its site block on the next rebuild.

Serving the web app direct-connect instead — browsers reaching the
polis services by plain port rather than through same-domain
subdomains — is a valid shape, but nothing pins it for you: pin each
service URL under `services.kallipai.web.runtimeConfig.services`, and
set the archeion's CORS allow-list (`corsOrigins`) to the origins the
browser actually uses. Prefer the proxy shape unless you have a reason
not to.

## HTTPS on a LAN or home network

Without reachable ports 80 and 443, ACME cannot issue certificates. On
a private network, Caddy's `tls internal` directive issues certificates
from Caddy's own local CA instead; browsers show a warning until the
host trusts that CA.

Add `tls internal` to each site block in the Minimal configuration —
a host's block then reads, for example:

```nix
services.caddy.virtualHosts."archeion.kallipai.lan".extraConfig = ''
  reverse_proxy 127.0.0.1:7100
  tls internal
'';
```

Repeat for the other subdomains — `lesche.`, `files.`, `instances.`,
and `web.` — if every subdomain is to serve https. The internal CA's
root certificate appears after Caddy's first start at
`/var/lib/caddy/.local/share/caddy/pki/authorities/local/root.crt`
(confirm it with `ls` on the target host). Distribute trust from there:

- Firefox keeps its own trust store, separate from the system bundle —
  import the file manually as an authority (Privacy & Security →
  Certificates → View Certificates → Authorities → Import) and do not
  rely on OS-level trust reaching it.
- For command-line tools (curl, git), copy the root into your
  configuration tree and add it to the system trust:

```sh
cp /var/lib/caddy/.local/share/caddy/pki/authorities/local/root.crt \
  /etc/nixos/kallipai-root.crt
```

```nix
security.pki.certificateFiles = [ ./kallipai-root.crt ];
```

The copy exists because the CA generates its root at runtime, while
`security.pki.certificateFiles` is read while the system trust bundle
is built — a direct reference to the `/var/lib` path cannot resolve.

## Deploy and verify

Switch into the new generation:

```sh
sudo nixos-rebuild switch
```

Check the five units:

```sh
systemctl status kallip-daemon kallip-archeion kallip-lesche \
  kallip-files kallip-instances
```

Then confirm each subdomain answers — `archeion.kallipai.lan`
for sign-up and login, `web.kallipai.lan` for the app, and the lesche,
files, and instances subdomains through the same proxy. A healthy
deployment: the app loads, you can sign up and sign in, and you can
create a first agent.

A missing internal-token file is not an error on the archeion's first
boot — it generates one. The lesche, files, and instances units require
the archeion and read that file at boot; if it has not appeared within a
short grace window the unit refuses to start, and
`journalctl -u kallip-instances` shows the path it waited for. An empty
token file fails every reader explicitly — delete the file to
re-provision rather than editing it by hand.

## Further options

The full option surface — per-service tuning, `adminTokenFile` and
`notifyTokenFile` — is described in the option declarations in
`nix/nixos-modules.nix`, with an option-level summary in the polis
NixOS module section of [container.md](reference/container.md). The
web site root helper is `nix/lib.nix`.
