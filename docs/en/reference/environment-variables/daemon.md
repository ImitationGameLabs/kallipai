---
title: Local daemon
description: Environment variables read by the local kallipai-daemon.
order: 60
---

Part of the [environment variable reference](index.md); this page covers
the desktop daemon.

| Variable                                   | Required    | Default                                     | Description                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| ------------------------------------------ | ----------- | ------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `KALLIPAI_DAEMON_RECORD_DIR`               | no          | `~/.local/state/kallipai/daemon/instances/` | The daemon's registration-record root: one `<slug>.json` record per managed instance (instance id, owning uid, target uid, workspace, user env, launch anchor, and the three roots that launch resolved: data, config and state). Every reader that needs one of those trees reads the record, so a log listing points at the recorded state root and a relaunch compares the environment it resolves against the recorded data root. A record written before the daemon stored the roots is completed once at startup; a record that pass cannot complete is left exactly as it is, and the commands that need that root refuse it with the keys that would supply one. Set verbatim; overrides the XDG state-home derivation. |
| `KALLIPAI_TAGMA_SLUG`                      | yes (tagma) | _(none)_                                    | Names the instance; the tagma resolves its data, config and state roots from it (see the rows below: the data and config roots are the slug-named tree under their home, the state root is `<state home>/kallipai`, and the slug names only the log leaf), and refuses to boot without it. Slug grammar: `[a-z0-9][a-z0-9-]*`, at most 64 characters. Injected by the daemon for managed instances; set it for direct runs (compose sets `main`). |
| `KALLIPAI_TAGMA_DATA_DIR`                  | no          | `<data home>/kallipai/tagmata/<slug>`       | The instance data root. Set, it is taken verbatim, must be absolute and wins over `XDG_DATA_HOME`; unset or empty the root is `<XDG_DATA_HOME>/kallipai/tagmata/<slug>`, and with that variable unset or empty `<home>/.local/share/kallipai/tagmata/<slug>`. A relative value is refused with the key named, and a boot that finds no rule naming a root refuses too. A managed launch hands down the daemon's own value when it has one and invents none; on a drop-to launch the root follows the target user's home. Requests cannot set it. |
| `KALLIPAI_TAGMA_STATE_DIR`                 | no          | `<state home>/kallipai`                     | The instance state root; its logs land in `<state root>/tagmata/<slug>/logs`. Resolved like the rows above: this key verbatim when absolute, else `<XDG_STATE_HOME>/kallipai`, else `<home>/.local/state/kallipai`. The daemon reads its own record root from the state home too, so in place the instance's logs sit beside the daemon's records, and on a drop-to launch they follow the target user. Requests cannot set it. |
| `KALLIPAI_TAGMA_CONFIG_DIR`                | no          | `<config home>/kallipai/tagmata/<slug>`     | The instance config root (`profiles.toml`, `exec_hooks.toml`, `settings.toml`). Resolved like the data root: this key verbatim when absolute, else `<XDG_CONFIG_HOME>/kallipai/tagmata/<slug>`, else `<home>/.config/kallipai/tagmata/<slug>`. A change of answer does not migrate anything between the old and the new tree. A relative value is refused with the key named. Requests cannot set it. |
| `KALLIPAI_TAGMA_ACCEPT_UNSAFE_RUN_AS_ROOT` | no          | unset                                       | The tagma real-root guard's only escape. A tagma booting with euid 0 in the initial user namespace (the host's real root; a mapped sandbox root sits in a nested namespace and is unaffected) refuses to start. Only the exact value `1` unlocks the boot, with a warning logged; every other value (empty, `0`, anything else) keeps the refusal. Meant for rootful containers without user-namespace remapping; elsewhere, prefer a dedicated unprivileged user.                                                  |
| `KALLIPAI_HARVEST_BASH`                    | no          | `/bin/bash`                                 | Bash used for the login-environment harvest (both launch forms; on drop-to launches it runs as the target user). An administrative constant: deployments without `/bin/bash` (NixOS) point it at a managed bash; nothing a request or a user environment supplies can move it.                                                                                                                                                                                                                                      |
| `KALLIPAI_DAEMON_SOCKET_GROUP`             | no          | _(none)_                                    | Control-socket access group: when set, the daemon hands the socket to this group (resolved through the group database at bind time) and widens the mode to 0660, so group members can drive the daemon.                                                                                                                                                                                                                                                                                                             |

On a drop-to launch (a root daemon starting an instance for a declared
user) the daemon constructs the instance's standard identity environment
from the passwd entry: `HOME`, `USER`, `LOGNAME`, `SHELL`,
`XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`,
and `XDG_RUNTIME_DIR` when the runtime directory exists. These keys are
daemon-owned: neither the harvested login environment nor request pairs
can override them. The instance resolves its three roots against those
pins, so they land in the target user's home, and a root key the
deployment set on the daemon is dropped for this form. Beyond the
identity pins the instance inherits the deployment's root variables and
the trace configuration below. The login-environment harvest is also
seeded with `XDG_RUNTIME_DIR` (the user's runtime directory, when it
exists), so session scripts that derive from it evaluate as they would
in a real login.

On an in-place launch the daemon passes down root variables only where
its own environment names them: the three own keys and
`XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`. It pins no root of
its own. A deployment that names a root in the daemon unit therefore
fixes it for every instance the daemon launches, and one that names
nothing leaves each instance to resolve from its own `HOME`. A root
variable the login harvest exports never reaches the instance env: a
profile file the instance's user owns must not decide where the instance
lands. For every other key the deployment wins over the profile, and the
profile wins over the seed. Every root resolves the same way: own key,
relative XDG value is ignored, and a relative own key is refused with
the key named. When nothing names a root the tagma refuses to boot, and
the refusal names the key or variable that supplies one.

The trace configuration is the one family the daemon hands over. When its
own environment carries `OTEL_EXPORTER_OTLP_ENDPOINT`, every instance it
spawns receives that endpoint, together with `OTEL_TRACES_SAMPLER` and
`OTEL_TRACES_SAMPLER_ARG` when they are set. The instance id follows the
generic key, not its source: an instance whose own environment carries
`OTEL_EXPORTER_OTLP_ENDPOINT` — the daemon's, or one a login shell exported —
receives
`OTEL_RESOURCE_ATTRIBUTES=service.instance.id=<slug>`, so several instances
of one build stay apart in the collector; an instance with no endpoint
receives no attribute. The daemon's value
replaces a same-named pair from any other channel, and a request cannot
name these keys: the allowlist admits `KALLIPAI_*`, `RUST_LOG` and
`PATH`. An `OTEL_RESOURCE_ATTRIBUTES` set by any other channel merges:
its attributes stay and `service.instance.id` is appended. With many
instances on one host, set `OTEL_TRACES_SAMPLER` to
`parentbased_traceidratio` and `OTEL_TRACES_SAMPLER_ARG` to a fraction:
each instance carries its own batch exporter, so sampling everything
spends CPU on traces nobody reads.

The record stores the three roots each launch resolved, and a relaunch
refuses to start a process when the environment it resolves no longer
names the recorded data root: the instance would otherwise read, write
and log in a tree the daemon does not record, scan or stop under. The
refusal prints both paths. A move that follows a record written by an
older daemon, which let a login profile's root key through, is settled
by putting that key on the daemon's unit and restarting the daemon, or
by adopting the tree the launch resolves so the record points at it.
