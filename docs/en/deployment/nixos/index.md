---
title: NixOS
order: 5
description: Deploying KallipAI hosts with the provided NixOS modules.
---

This guide walks through deploying the KallipAI platform form on a NixOS
host: the daemon as a system service, the four polis services (archeion:
identity control plane; lesche: relay data plane; files: content transfer;
instances: web instances proxy)
behind the host's reverse proxy, and the web app. The container form (arion
compose, Nix-native docker-compose) is a separate, complementary way to
run the platform; this
guide walks the NixOS module bring-up step by step. The
module itself is `nix/nixos-modules.nix`, and its option descriptions are
the authoritative reference for everything this guide summarizes.

The guide is split into three chapters, in the order a first
deployment reads them:

- [Minimal deployment](minimal.md): prerequisites, the module import, a
  minimal configuration, and the first bring-up and login.
- [HTTPS configuration](https.md): the two upgrade paths
  when ACME cannot issue.
- [Service model and operations](operations.md): tokens, launch
  identities, and the real-root guard underneath the deployment.

The full option surface (per-service tuning, `adminTokenFile` and
`notifyTokenFile`) is described in the option declarations in
`nix/nixos-modules.nix`. The web site root helper
is `nix/lib.nix`.
