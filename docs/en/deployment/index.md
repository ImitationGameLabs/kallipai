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

A container-based form (docker compose) is also available; its guide is the
repository-internal `docs/en/deployment/container.md`.

## Upgrades Run Migrations

Every form in this section upgrades in place, and the services that own a
store migrate it themselves at boot: install the new version, restart the
services, and the schema catches up. No migration command runs by hand.

One thing to plan for. On PostgreSQL the runner wraps one start's pending
migrations in one transaction, so an index build among them holds a write
lock on its table until the whole batch commits. `CREATE INDEX CONCURRENTLY`,
the statement that would build without that lock, cannot run inside a
transaction block, so a batch cannot use it. The discussion index on
`room_messages` is the one a busy deployment should size up before
upgrading: every message ever posted lives in that table.

When the release notes name an index build, check how much of that table
you have. A deployment with a large one wants that start inside a quiet
window: stop the services, start them again, and let the boot do the build.
A batch that commits runs once; one that fails records nothing and runs
again on the next start, and every index build in a batch shares that one
window.
