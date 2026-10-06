---
title: Installing on Linux (FHS)
description: "Install the KallipAI tarball on a conventional x86_64 Linux distribution: user install, systemd setup, upgrades, and uninstall."
order: 15
---

This guide covers the FHS install form: a self-contained tarball that runs
on any x86_64 Linux distribution with glibc 2.35 or newer. For NixOS hosts,
prefer the [NixOS modules](nixos/index.md); the FHS form targets
Ubuntu 22.04+, Debian 12+, Fedora 36+, Arch, and RHEL 10+.
The v1 FHS form covers the binaries and the systemd units; reverse-proxy
and TLS termination for the polis listeners, and the web app, stay
outside its scope.

## Prerequisites

- A 64-bit x86_64 Linux machine with glibc 2.35 or newer. The installer
  checks both and refuses loudly on a mismatch.
- `curl`, `tar`, and `sha256sum` (all present on the supported
  distributions by default).
- For the systemd install: root access and a running systemd.

## Quick start

The default install is per-user: no root, no systemd units.

Downloading mode must name a release: pass --version (any released
version works once releases exist).

```bash
curl -fsSL https://raw.githubusercontent.com/kallipai/kallipai/main/install.sh | bash -s -- --version 1.0.0
```

Reading a script before running it is a reasonable habit, so the same
install in review-first form:

```bash
curl -fsSL https://raw.githubusercontent.com/kallipai/kallipai/main/install.sh -o install.sh
less install.sh
bash install.sh
```

To install from a local tarball instead of downloading (the release
tarball ships with a `.sha256` sidecar, which the installer verifies
before unpacking):

```bash
bash install.sh --tarball ./kallipai-1.0.0-linux-x86_64.tar.gz
```

## What goes where

The versioned install root and the data root are separate, and the
installer only ever writes to the former:

- Payload: `~/.local/lib/kallipai/<version>/`, one directory per version.
- Links: `~/.local/bin/<name>`, pointing into the payload. If this
  directory is not on your `PATH`, the installer prints the line to add
  for your shell.
- Data: `~/.local/share/kallipai`, created by the tools at runtime, never
  by the installer.

## Upgrading

Installing is idempotent. Run the same installer with a newer tarball and
the links move to the new version; the old version directory is removed
afterwards, unless a process is still running from it (the installer
reports those rather than killing anything).

## Uninstalling

```bash
bash install.sh --uninstall
```

Uninstall removes the payload tree and the links that point into it
(links with any other target are left alone). Your data in
`~/.local/share/kallipai` is kept.
Uninstalling a system-level install follows the same pattern with
`sudo bash install.sh --with-systemd --uninstall`: root removes the
payload, the links, and the five unit files, and keeps `/var/lib/kallipai`
(the services' state) plus the system users and groups, printing manual
cleanup hints.

## System install with systemd

For a host-wide install with services, add `--with-systemd`. It needs
root and the repository checkout (the unit templates live under
`nix/install/systemd/`):

```bash
sudo bash install.sh --tarball ./kallipai-1.0.0-linux-x86_64.tar.gz --with-systemd
```

This installs the payload under `/usr/local/lib/kallipai/<version>/`,
links into `/usr/local/bin`, creates dedicated system users, and renders
five units into `/etc/systemd/system/`:

- `kallipai-daemon` (runs as root; `KillMode=process` so running tagmata
  survive a daemon restart)
- `kallipai-archeion`, `kallipai-lesche`, `kallipai-files`,
  `kallipai-instances` (one dedicated user each)

Units are enabled and already-active units are restarted through an
upgrade. Re-running the same command with a newer tarball upgrades in
place.

The polis services expect PostgreSQL with one database and role per
service (peer authentication over the local unix socket), mirroring the
NixOS module defaults; the database itself is provided by the deployer.
For example, on a stock Debian-family host:

```bash
sudo -u postgres createuser kallipai-archeion
sudo -u postgres createdb -O kallipai-archeion kallipai-archeion
```

Repeat the pair for `kallipai-lesche` and `kallipai-files`; peer
authentication rides the local unix socket.
Optional settings go in drop-ins (`systemctl edit <unit>`); each unit
template carries an example in its header comment.

## Without systemd

On a host without a running systemd, `--with-systemd` still installs the
payload and links, renders the units, and prints a notice instead of
enabling them. Run the binaries directly or manage them with your own
init system.
