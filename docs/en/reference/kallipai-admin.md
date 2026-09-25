---
title: kallipai-admin CLI reference
description: CLI reference for the headless archeion admin tool.
order: 76
---

`kallipai-admin` is a headless operator CLI for the archeion relay: it drives
the `/admin/*` surface (users, passkeys, enrollment codes, admin-token
lifecycle) over HTTP, authenticated with the `KALLIPAI_ARCHEION_ADMIN_TOKEN`
environment variable. The token is env-only by design: a flag would leak the
secret into `ps`, `/proc/<pid>/cmdline`, and shell history.

## Shell completions

`kallipai-admin` ships a hidden `generate` verb that prints a completion
script for a given shell:

```bash
kallipai-admin generate zsh    # or bash, fish, elvish, powershell
```

On nix installs (the `kallipai-admin`, `kallipctl`, or `workspace` package)
the scripts are generated at build time and installed automatically: bash,
zsh, and fish completions land in the standard profile locations, and all
five shells are kept under `share/kallipai/completions/kallipai-admin/` for
manual installation elsewhere.

For a non-nix install, wire the script into your shell's startup (zsh
example, using a user-owned completion directory):

```bash
mkdir -p ~/.zfunc && kallipai-admin generate zsh > ~/.zfunc/_kallipai-admin
# in ~/.zshrc, before compinit:
fpath=(~/.zfunc $fpath)
autoload -Uz compinit && compinit
```
