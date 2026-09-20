---
order: 40
title: Service model and operations
description: How the services run as systemd units, and day-to-day operations like token rotation.
---

This chapter covers the service model underneath the deployment: the
internal token, launch identities, and the failure modes worth
recognizing at boot.

## The Internal Token (Self-Managed)

The polis services authenticate to each other with one shared secret,
provisioned by the control plane into its state directory
(`/var/lib/kallipai/archeion/internal-token`, mode 0640, readable by
the `kallipai-polis` group the module creates). The other three
services read the same file, so all four agree on one value for the
lifetime of the deployment.

Secret state is filed by lifetime: `/etc` holds administrator-owned
static configuration (a pinned admin token), `/run` holds volatile
runtime state, and `/var/lib` holds service-owned persistent state
(the internal token).

To rotate the internal token: stop the four polis services, delete the
file, start the archeion (a fresh value is generated), then start the
other three. The group restart keeps every service on the same
generation.

### Users, Launch Identities, and the Real-Root Guard

The module's daemon unit runs as `root` (a system service that reads
every declared user's passwd entry), while the declared `tagmaUsers`
are the only launch identities instances ever run as. A root daemon
refuses an inferred in-place launch (the error asks for `--user`), so
every `adopt` and `start` on NixOS names one of the declared users
explicitly; nothing the platform hosts runs as the host's real root,
and a tagma started directly as real root refuses to boot outright
(the escape hatch is `KALLIP_TAGMA_ACCEPT_UNSAFE_RUN_AS_ROOT=1`, not
meant for this deployment form).

The declared users are provisioned with a home directory and linger:
`/home/<user>` holds the instance's XDG config, data, and state
trees, and logind pre-creates `/run/user/<uid>` at boot: the spawned
instance's `XDG_RUNTIME_DIR`, with no per-instance setup.
Service-owned persistent state (the polis services'
stores and the internal token) stays under `/var/lib/kallipai`,
separate from the per-user homes: the same lifetime split the
internal-token section describes.

## Troubleshooting the First Boot

A missing internal-token file is not an error on the archeion's first
boot; it generates one. The lesche, files, and instances units require
the archeion and read that file at boot; if it has not appeared within a
short grace window the unit refuses to start, and
`journalctl -u kallip-instances` shows the path it waited for. An empty
token file fails every reader explicitly; delete the file to
re-provision rather than editing it by hand.
