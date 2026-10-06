---
title: Deployment
order: 1
description: "How to deploy KallipAI: the NixOS modules, or a conventional Linux install."
---

Each chapter under this section is one way to run the platform, written
as a step-by-step guide for its platform form.

- [NixOS](nixos/index.md): deploy on a NixOS host with the provided NixOS
  modules: a minimal bring-up, HTTPS, and day-to-day
  operations.
- [Install on Linux](install.md): install the tarball on a conventional
  x86_64 distribution: per-user by default, with an optional systemd
  system-level form.

A container-based form (arion compose) is also available; its guide is
the repository-internal `docs/en/deployment/container.md`.
