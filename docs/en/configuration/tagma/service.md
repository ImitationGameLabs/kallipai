---
title: Tagma service
description: Environment variables that configure the tagma server.
order: 40
---

Part of the [Configuration reference](../index.md); this page covers the server process itself: listen address, data and
skill roots, and logging.

Tagma server variables:
[Tagma](../../reference/environment-variables/tagma.md).

## `ADVERTISE_URL` vs `TAGMA_URL`

These serve related but distinct purposes:

- **`KALLIP_ADVERTISE_URL`**: configured by the operator. Tells the tagma "this
  is the URL others should use to reach you." The tagma injects this value into
  child processes.
- **`KALLIP_TAGMA_URL`**: consumed by clients (CLI). Tells them "where is
  the tagma." Automatically set from `ADVERTISE_URL` by the tagma at startup.

In the common case (everything on localhost) they have the same value. They
diverge in container or reverse-proxy setups where the internal listen address
differs from the externally reachable URL.

### Data and Skills

| Variable             | Required | Default                              | Description                                                                                                                                                                                                                                                                                                                                                                        |
| -------------------- | -------- | ------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| (data root)           | no       | _(derived)_                                    | Derived from `KALLIP_TAGMA_SLUG` under the platform data dir (see [Local daemon](../../reference/environment-variables/daemon.md)); there is no per-instance data-dir variable to set. The runtime writes `agents/` (with the `active/`, `inactive/`, and `archived/` life-stage directories) and `skills/` directly under it.                                                                                                                                                                                                                                    |
| `KALLIP_SKILLS_ROOT` | no       | Instance data root's `skills/`       | Direct path to the shared skill directory. Used as-is (no suffix appended).                                                                                                                                                                                                                                                                                                             |
| `KALLIP_SKILLS_SEED` | no       | _(unset; nix wrapper)_              | Read-only tree of bundled skill defaults (a nix store path). On the tagma's first boot, when the shared skill directory is empty, its contents are copied into it. The target is `KALLIP_SKILLS_ROOT` if set, else the instance data root's `skills/`; `KALLIP_SKILLS_ROOT` only relocates the target, it does not disable seeding. Skipped when the target is already non-empty (never clobber). Under a nix install the workspace build ships this default already via its `kallip-tagma` wrapper (`--set-default`: it applies only when the variable is unset, so an explicit env value or the container image Env still wins). An explicitly empty value disables seeding: the wrapper keeps it as-is and the tagma filters it out. |

### Logging

| Variable   | Required | Default | Description
| ---------- | -------- | ------- | -------------------------------------------------------------------------------------------------------------------
| `RUST_LOG` | no       | `info`  | Standard env-filter syntax. Controls log verbosity for the tagma. Example: `kallip_client=debug`.

Platform service log directories:
[Platform services](../../reference/environment-variables/platform-services.md).
