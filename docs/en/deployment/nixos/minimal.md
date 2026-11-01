---
title: Minimal deployment
description: Bring up the whole platform on one NixOS host with a single configuration file.
order: 10
---

## Prerequisites

- A NixOS host with flakes enabled
  (`nix.settings.experimental-features = [ "nix-command" "flakes" ]`).

## Import the Module

Add kallipai as a flake input, then include
`inputs.kallipai.nixosModules.kallipai` in your NixOS
configuration's modules:

```nix
{
  inputs.kallipai.url = "github:ImitationGameLabs/kallipai";

  outputs = { nixpkgs, ... }@inputs: {
    nixosConfigurations."<myhost>" = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        inputs.kallipai.nixosModules.kallipai
        ./configuration.nix
      ];
    };
  };
}
```

## Minimal Configuration

```nix
{ config, ... }:
let
  domain = "kallipai.lan";
  ports = config.services.kallipai.polis.ports;
in
{
  services.kallipai = {
    inherit domain;
    tls = false;
    daemon.enable = true;
    polis.enable = true;
    web.enable = true;
  };

  services.caddy = {
    enable = true;
    virtualHosts = {
      # The web app: the SPA with the runtime config baked in.
      "http://app.${domain}".extraConfig = ''
        root * ${config.services.kallipai.web.distWithRuntimeConfig}
        try_files {path} /index.html
        file_server
      '';

      # The platform's single API face: path-routed per service.
      "http://api.${domain}".extraConfig = ''
        # The archeion's internal surface stays off the public face.
        @internal path /v1/archeion/internal /v1/archeion/internal/*
        handle @internal {
          respond 404
        }
        handle_path /v1/archeion/* {
          reverse_proxy 127.0.0.1:${toString ports.archeion}
        }
        handle_path /v1/lesche/* {
          reverse_proxy 127.0.0.1:${toString ports.lesche} {
            # lesche serves SSE streams; flush_interval -1 disables
            # response buffering so events reach clients immediately.
            flush_interval -1
          }
        }
        handle_path /v1/keryx/* {
          reverse_proxy 127.0.0.1:${toString ports.keryx} {
            # keryx streams like the lesche; flush_interval -1 disables
            # response buffering so events reach clients immediately.
            flush_interval -1
          }
        }
        handle_path /v1/instances/* {
          reverse_proxy 127.0.0.1:${toString ports.instances}
        }
        @files path /v1/files /v1/files/*
        handle @files {
          uri strip_prefix /v1/files
          reverse_proxy 127.0.0.1:${toString ports.files}
        }
        # The model gateway's own API as the sixth /v1 service segment.
        # Everything under the segment is the gateway's management face
        # (profile reads, the admin family, health, metrics), so the
        # stripped path routes on the management port. Every route on
        # that face carries its own family credential (an admin token or
        # a proxy key) except the unauthenticated health and metrics
        # probes; the edge adds none.
        handle_path /v1/model-gateway/* {
          reverse_proxy 127.0.0.1:${toString config.services.kallipai.polis.ports.model-gateway}
        }
      '';

      # The model gateway: its own host for the OpenAI wire (clients
      # configure a /v1 base; the edge strips it before the gateway).
      "http://model-gw.${domain}".extraConfig = ''
        handle_path /v1/* {
          # Streaming wire (chat completions stream): like the lesche
          # segment, disable buffering so tokens reach clients live.
          reverse_proxy 127.0.0.1:${toString config.services.kallipai.polis.model-gateway.forwardPort} {
            flush_interval -1
          }
        }
        # The fallback carries the same streaming surface.
        reverse_proxy 127.0.0.1:${toString config.services.kallipai.polis.model-gateway.forwardPort} {
          flush_interval -1
        }
      '';
    };
  };

  networking = {
    firewall = {
      allowedTCPPorts = [
        80
      ];
    };

    hosts = {
      "127.0.0.1" = [
        "app.kallipai.lan"
        "api.kallipai.lan"
        "model-gw.kallipai.lan"
      ];
    };
  };
}
```

## Rootless Usage

To drive the daemon with `kallipctl`, admit the user to the
daemon's socket group: the control socket lives at
`/run/kallipai/daemon.sock` and is gated to the `kallipai`
group (replace `<username>` with your username):

```nix
users.users."<username>".extraGroups = [ "kallipai" ];
```

## Deploy and Verify

Once the NixOS configuration has built and switched to the new generation, verify the deployment as follows:

Check the seven units:

```sh
systemctl status kallipai-daemon kallipai-archeion kallipai-lesche \
  kallipai-keryx kallipai-files kallipai-instances kallipai-model-gateway
```

Services that write log files keep daily-rolled `<service>.<date>.log`
files under `/var/log/kallipai/<service>/` (seven kept). That directory
is mode `0750`, owned by the service user with
`services.kallipai.daemon.group` (default `kallipai-polis`) as its
group, so read it as a member of that group or with `sudo`.

Then confirm the three hosts answer: `api.kallipai.lan` for the
platform, `app.kallipai.lan` for the web app, and
`model-gw.kallipai.lan` for the model gateway. A healthy deployment: the
app loads, you can sign up and sign in, and you can create a first
agent. The gateway exposes a health endpoint:

```sh
curl http://model-gw.kallipai.lan/health
```

The `/metrics` exposition rides the management port, not the
model-gw host: reach it on the host itself at
`http://127.0.0.1:7500/metrics`, or through the api edge at
`http://api.kallipai.lan/v1/model-gateway/metrics`. It is the
Prometheus text of the forwarding observability counters, with no
credential: a probe that must survive credential outages.

The NixOS module exports the scrape job for that face as a read-only
list: `config.services.kallipai.polis.scrapeConfigs`. The exported job
scrapes every 30 seconds. The module never starts a recorder; wire the
list into the host's Prometheus on its own options (append more jobs
with `++`, retention follows `services.prometheus.retentionTime`):

```nix
services.prometheus = {
  enable = true;
  scrapeConfigs = config.services.kallipai.polis.scrapeConfigs;
};
```

## Gateway Admin Face

The model gateway's management face carries two credential families on
one port: the proxy key authorizes distribution reads and the
forwarding face, and the platform identity authorizes the admin family.
The families share no path and no failure path, so one credential says
nothing about the other.

The admin credential is the platform identity (the archeion). A request
passes either with an archeion admin bearer (the machine and CLI
channel) or with the local admin's platform session cookie (the browser
channel: the admin page signs in with the platform account). A valid
non-admin identity is authenticated but unauthorized (403); an invalid
or absent credential is rejected (401); an archeion that cannot be
reached, answers a status outside the call's contract, or sends a body
that will not decode fails closed (503), and an answer that contradicts
the wire contract (a bearer with no credential record) fails closed
(500). With no archeion wiring configured the whole face is closed, and the
distribution and forwarding faces run unaffected.

The browser channel is hardened as a pair: session cookies are HttpOnly
and SameSite=Strict, and state-changing requests must carry a custom
header a cross-origin browser cannot synthesize. Sessions are opaque
hashed credentials server-side. Attribution follows the credential:
every admin change lands in the audit trail with the operator that made
it, and profiles and sets record the account that owns them.

## Admin Token

With `adminTokenFile` unset, the archeion mints an admin token into its
state directory (`/var/lib/kallipai/archeion/admin-token.env`, mode 0600)
on first boot and reads it, never rewriting it, after that. No command
prints the current value: the server never returns it, so rotation (which
answers with a new one) is the only way through:

```sh
sudo kallipai-admin admin-token reset
```

The reset authenticates with the current token, taken from
`KALLIPAI_ARCHEION_ADMIN_TOKEN` or, when that is unset, from the state
file, and it reads that file locally before calling the server as well.
So it runs on the archeion host with no export first (an exported token
still wins, and `sudo -E` keeps it). It prints the new value once: nothing
prints it afterwards, so capture that output.

For routine work against the platform, give `kallipctl` a personal access
token instead of the admin token: create one in the web UI with the
strings that work needs, write it to
`$XDG_CONFIG_HOME/kallipai/access-token` (mode 0600), and give
`kallipctl spawn <slug> <workspace> --polis <origin>` the token. See the
kallipctl reference for the whole flow.

With `adminTokenFile` set, the operator-pinned token is used as-is and
rotation is refused.

Switching from minted to pinned: set `adminTokenFile` and redeploy.
The archeion then uses the pinned value; the state file stays on disk,
nothing mints it any more, and `kallipai-admin` would still fall back to
it whenever the environment variable is unset. Delete the file rather
than leave a stale token around.
