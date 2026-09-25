---
title: kallipctl CLI reference
description: CLI reference for the operator-side instance management tool.
order: 75
---

`kallipctl` manages local kallip instances through the daemon's control
socket: spawn, adopt, start/stop/restart, persisted env, logs, and blob-store
maintenance. See [deployment](../deployment/nixos/minimal.md) for the daemon
setup; the socket's `0600` mode is the auth.

## Shell completions

`kallipctl` ships a hidden `generate` verb that prints a completion script
for a given shell:

```bash
kallipctl generate bash    # or zsh, fish, elvish, powershell
```

On nix installs (the `kallipctl`, `kallip-admin`, or `workspace` package)
the scripts are generated at build time and installed automatically: bash,
zsh, and fish completions land in the standard profile locations, and all
five shells are kept under `share/kallipai/completions/kallipctl/` for
manual installation elsewhere.

For a non-nix install, wire the script into your shell's startup (bash
example):

```bash
kallipctl generate bash > ~/.local/share/bash-completion/completions/kallipctl
```

## Dynamic completions (optional)

The installed scripts complete subcommands and flags. For live value
completion too (instance slugs, queried from the daemon as you type), source
the dynamic hook instead. It degrades to no candidates when the daemon is
unreachable, so typing is never blocked:

```bash
echo "source <(COMPLETE=bash kallipctl)" >> ~/.bashrc
echo "source <(COMPLETE=zsh kallipctl)" >> ~/.zshrc
```
