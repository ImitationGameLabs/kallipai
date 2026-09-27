---
title: Local daemon
description: Environment variables read by the local kallipai-daemon.
order: 60
---

Part of the [environment variable reference](index.md); this page covers
the desktop daemon.

| Variable                                   | Required    | Default                                     | Description                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| -------------------------                  | --------    | -------------------------------             | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------                                                                                                                                                                                                                                                            |
| `KALLIPAI_DAEMON_RECORD_DIR`               | no          | `~/.local/state/kallipai/daemon/instances/` | The daemon's registration-record root: one `<slug>.json` record per managed instance (instance id, owning uid, target uid, workspace, user env, launch anchor, and the pointer at the instance data directory). Set verbatim; overrides the XDG state-home derivation.                                                                                                                                                                                             |
| `KALLIPAI_TAGMA_SLUG`                      | yes (tagma) | _(none)_                                    | Names the instance; the tagma derives its data root (`<data home>/kallipai/tagmata/<slug>`), config root, and logs from it, and refuses to boot without it. Slug grammar: `[a-z0-9][a-z0-9-]*`, at most 64 characters. Injected by the daemon for managed instances; set it for direct runs (compose sets `main`).                                                                                                                                                 |
| `KALLIPAI_TAGMA_ACCEPT_UNSAFE_RUN_AS_ROOT` | no          | unset                                       | The tagma real-root guard's only escape. A tagma booting with euid 0 in the initial user namespace (the host's real root; a mapped sandbox root sits in a nested namespace and is unaffected) refuses to start. Only the exact value `1` unlocks the boot, with a warning logged; every other value (empty, `0`, anything else) keeps the refusal. Meant for rootful containers without user-namespace remapping; elsewhere, prefer a dedicated unprivileged user. |
| `KALLIPAI_HARVEST_BASH`                    | no          | `/bin/bash`                                 | Bash used for the login-environment harvest (both launch forms; on drop-to launches it runs as the target user). An administrative constant: deployments without `/bin/bash` (NixOS) point it at a managed bash; nothing a request or a user environment supplies can move it.                                                                                                                                                                                     |
| `KALLIPAI_DAEMON_SOCKET_GROUP`             | no          | _(none)_                                    | Control-socket access group: when set, the daemon hands the socket to this group (resolved through the group database at bind time) and widens the mode to 0660, so group members can drive the daemon.                                                                                                                                                                                                                                                            |

On a drop-to launch (a root daemon starting an instance for a declared
user) the daemon constructs the instance's standard identity environment
from the passwd entry: `HOME`, `USER`, `LOGNAME`, `SHELL`,
`XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`,
and `XDG_RUNTIME_DIR` when the runtime directory exists. These keys are
daemon-owned: neither the harvested login environment nor request pairs
can override them, and the instance inherits nothing else from the
daemon's own environment.
The login-environment harvest is also seeded with `XDG_RUNTIME_DIR`
(the user's runtime directory, when it exists), so session scripts
that derive from it evaluate as they would in a real login.
