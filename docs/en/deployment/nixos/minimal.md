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
        handle_path /v1/instances/* {
          reverse_proxy 127.0.0.1:${toString ports.instances}
        }
        @files path /v1/files /v1/files/*
        handle @files {
          uri strip_prefix /v1/files
          reverse_proxy 127.0.0.1:${toString ports.files}
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
      ];
    };
  };
}
```

## Rootless Usage

To drive the daemon with `kallipctl`, admit the user to the
daemon's socket group: the control socket lives at
`/run/kallipai/daemon.sock` and is gated to the `kallipai-daemon`
group (replace `<username>` with your username):

```nix
users.users."<username>".extraGroups = [ "kallipai-daemon" ];
```

## Deploy and Verify

Once the NixOS configuration has built and switched to the new generation, verify the deployment as follows:

Check the five units:

```sh
systemctl status kallip-daemon kallip-archeion kallip-lesche \
  kallip-files kallip-instances
```

Then confirm the two hosts answer: `api.kallipai.lan` for the
platform and `app.kallipai.lan` for the web app. A healthy deployment: the app
loads, you can sign up and sign in, and you can create a first agent.

## Admin Token

With `adminTokenFile` unset, the archeion places a generated admin
token in its runtime directory
(`/run/kallipai/archeion/admin-token.env`, mode 0600).
Read it as follows:

```sh
sudo cat /run/kallipai/archeion/admin-token.env
```
