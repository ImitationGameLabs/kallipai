---
title: kallipctl CLI reference
description: CLI reference for the operator-side instance management tool.
order: 75
---

`kallipctl` manages local kallip instances through the daemon's control
socket: spawn, adopt, start/stop/restart, persisted env, logs, and blob-store
maintenance. See [deployment](../deployment/nixos/minimal.md) for the daemon
setup; the socket's `0600` mode is the auth.
For a polis service's own log file, `logs --service <name>` tails the
rolling files under `/var/log/kallipai/<service>` through the daemon:
the daemon reads the service user's files for the caller, so no sudo
is needed. Bare `kallipctl logs` lists the services that have logs.

## Shell completions

Completion is dynamic: the hooks query the daemon live as you type
(instance slugs, service names) and degrade to no candidates when the
daemon is unreachable, so typing is never blocked:

```bash
echo "source <(COMPLETE=bash kallipctl)" >> ~/.bashrc
echo "source <(COMPLETE=zsh kallipctl)" >> ~/.zshrc
```

On nix installs (the `kallipctl` or `workspace` package) these hook
scripts are generated at build time and installed
automatically: bash, zsh, and fish land in the standard profile
locations, and all five shells are kept under
`share/kallipai/completions/kallipctl/`. Each hook script embeds the
nix store path of its binary, so copying one to another host leaves it
silent. Regenerate it on that host instead.

The hidden `generate` verb prints the static (AOT) scripts: word lists
baked at build time, no daemon queries. The nix install never uses
them for kallipctl; they suit sealed environments that cannot run the
CLI during completion:

```bash
kallipctl generate bash    # or zsh, fish, elvish, powershell
```
