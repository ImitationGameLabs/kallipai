---
title: kallipctl CLI reference
description: CLI reference for the operator-side instance management tool.
order: 75
---

`kallipctl` manages local kallip instances through the daemon's control
socket: spawn, adopt, start/stop/restart, persisted env, logs, and blob-store
maintenance, plus the host's own platform register (`kallipctl polis`, which
needs no daemon). See [deployment](../deployment/nixos/minimal.md) for the daemon
setup; the socket's `0600` mode is the auth.

## Spawn with a personal access token

Remote instances enroll with the platform through a single-use enrollment
code. `kallipctl spawn <slug> <workspace> --polis <origin>` mints one with
an access token and hands the instance only the code. Both positionals are
required; a relative workspace is resolved against the current directory:

```bash
kallipctl spawn dev /srv/kallipai/dev --polis https://kallip.example.com
```

The token is read from `$XDG_CONFIG_HOME/kallipai/access-token` (`~/.config`
when the variable is unset) unless `--token-file` names another path. Create
it in the web UI with `archeion:tagma:write`: new tokens start read-only, and
a token without that string cannot mint. The file must stay owner-readable
only:

```bash
chmod 600 ~/.config/kallipai/access-token
```

`--polis` is the platform edge origin: the value the instance receives as
`KALLIPAI_POLIS_URL`, and the base the mint appends `/v1/archeion` to. It is
not a service's bare address. It must be an http or https URL with no query,
no fragment, and no user name or password; plain http is accepted for a
loopback origin (a loopback IP, `localhost`, or any name under `.localhost`),
because the access token and the code it mints would otherwise cross the
network in the clear. A plaintext origin off this host needs
`--allow-plaintext`, a top-level option placed before the subcommand. `spawn`
with `--polis`, `polis add`, and every `polis` verb that spends a token read
it, because the address rule runs before the token is touched. When the flag
lets a plaintext origin through, the command names the risk on stderr and
proceeds.

A plaintext edge on a trusted network needs the flag, before the subcommand:

```bash
kallipctl --allow-plaintext spawn dev /srv/kallipai/dev --polis http://kallipai.lan:7100
```

`--polis` writes `KALLIPAI_POLIS_URL` into the instance environment. Passing
`-e KALLIPAI_POLIS_URL` or `-e KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE` next to
`--polis` is refused rather than resolved by precedence. Without `--polis`,
no enrollment variables are written and the spawn works offline.

The mint runs inside `kallipctl`, never inside the instance: the token stays
on the operator's machine. Each failure names its fix:

| Failure | Meaning |
| --- | --- |
| no access token at PATH | create the token, then write it there |
| token file PATH is readable by other users | `chmod 600` it |
| token file PATH is empty | write the access token into it |
| archeion rejected the access token | invalid, expired, or revoked |
| the access token lacks archeion:tagma:write | add the string |
| the token belongs to the admin console or a tagma | mint with a personal access token |
| archeion refused the request: 403 MESSAGE | it forbade the mint without a code to act on |
| too many live pending enrollment codes | revoke unused ones in the UI |
| archeion refused to mint an enrollment code: STATUS MESSAGE | the platform answered with a failure status; the text after it is the platform's own |
| archeion at ORIGIN did not answer within 30s | it answers on the network but sends nothing; 30s is the CLI's built-in deadline, with no flag to change it |
| cannot reach archeion at ORIGIN | check the URL and the network |
| archeion answered without an enrollment code | the code came back blank |
| minting an enrollment code: DETAIL | a 2xx arrived and this CLI could not read the body, so nothing came back to act on |
| --polis must be an http or https URL | the mint speaks HTTP(S) only |
| --polis must be an origin, without a query or fragment | pass scheme://host[:port] |
| --polis must be an origin, without a path | pass scheme://host[:port] |
| --polis must not carry a user name or password | drop the userinfo |
| --polis URL is not loopback and not https | the token would cross the network in the clear; pass --allow-plaintext to proceed anyway |

## Platform registry

`kallipctl polis` records the platforms this host talks to, one name each, and
asks a platform what its owner has registered there. The register is a file of
your own, `$XDG_CONFIG_HOME/kallipai/polis-registry.toml`, mode `0600`; nothing
here needs the daemon, so the verbs work while it is down.

| Verb | Does |
| --- | --- |
| `polis add NAME URL [--replace]` | records the address as an origin, normalized |
| `polis remove NAME` | drops the record and nothing else |
| `polis list` | name, address, and whether a token is stored; reads the file only |
| `polis token set NAME [--token-file PATH]` | stores the platform's access token |
| `polis status NAME` | the platform's verdict plus the tagmas registered for its owner |
| `polis revoke NAME TAGMA-ID` | revokes one registration on the platform |

Names follow the instance slug grammar: lowercase letters, digits and `-`. One
name per platform and one platform per name: a second name for a recorded
address is refused, and pointing a name at a new address needs `--replace`.

The address is an origin, the same rule `--polis` applies (http or https, no
path, query, fragment or userinfo), and a plaintext origin off this host needs
`--allow-plaintext` here too. `list` prints a masked token; the secret itself
never prints. `token set` reads `--token-file` when given and stdin otherwise
(a hidden prompt on a terminal), and spends one list call before it writes: a
token the platform refuses is not stored.

A token needs `archeion:tagma:read`. Revoking registrations and minting an
enrollment code (`spawn --polis`) both need `archeion:tagma:write` too, since a
newly minted token is read-only by default. `add`, `remove`, and `list` touch
only the register file and never the network.

`status` prints one line per row, the tagma id first so it can be copied
straight into `revoke`:

```text
local  http://api.example.com  reachable  token=accepted
tagma-1  enrolled  created=2026-11-01T09:12:00Z
tagma-2  pending  created=2026-11-11T17:40:00Z  expires=2026-11-11T18:10:00Z
```

A token the platform rejects still prints `reachable`, and the line under it
says what the platform said. `revoke` on an id the platform does not hold
prints `TAGMA is not registered on NAME` and exits 0, because that is the
outcome it asked for.

## Entries inside a running instance

`polis add` records a platform this host knows. `polis add-entry` writes one
entry into one instance's own `polis.toml`, enrolls it, and starts its relay.
The instance verbs need the daemon, to learn where the instance listens, and
they need the instance running.

| Verb | Does |
| --- | --- |
| `polis entries SLUG` | the entries inside that instance, plus whether it takes its entries from the environment instead |
| `polis add-entry SLUG NAME (--polis NAME \| --url ORIGIN) [--code CODE] [--token-file PATH]` | adds one entry, mints a code unless one is given, and starts its relay |
| `polis remove-entry SLUG NAME [--forget]` | drops one entry and stops its relay; `--forget` also deletes its credentials |
| `polis mint-code NAME` | mints an enrollment code here and prints it with its expiry and the pending tagma it created |

`mint-code` is the exception among them: it asks the instance nothing, so it
needs neither the daemon nor a running instance, only the register and the
platform.

`add-entry` normally mints the code itself and redeems it inside the same
call, so the code never reaches a shell history and its thirty-minute window
never becomes yours to watch:

```bash
kallipctl polis add-entry alpha local --polis edge
```

Before it mints, it asks the instance what it already holds: a name the
instance already holds credentials for, or an instance that takes its entries
from the environment, is refused without spending anything; an entry without
credentials is what a code repairs. `--url` is judged like a recorded
address, so a plaintext one needs `--allow-plaintext`. With `--polis` the
address, and the token to mint with, come from the register; a platform that
is not registered goes in by `--url`, together with `--code` or
`--token-file`. A code you already minted with `polis mint-code` is passed
with `--code`.

`add-entry` reports the entry head, then the instance's own note:

```text
local  https://edge.example.com  enrolled=on  tagma=t-9f2
relay connected; the gateway's target list is still the one from startup, so this entry joins it on the next start
```

A run that minted names the pending tagma and the code's expiry on stderr
before the entry is written, so a failure past that point still leaves the
handle `polis revoke NAME TAGMA` takes.

`polis entries` prints one line per entry, with the two run states spelled out
as words:

```text
local  https://edge.example.com  enrolled=on  tagma=t-9f2  relay=on  gateway=on
```

`gateway=off` means the gateway was started before this entry, so the entry
joins on the instance's next boot.

The instance is reached over its own loopback port, with the operator token
`kallipctl operator-token show SLUG` points at; an instance started with
`KALLIPAI_OPERATOR_TOKEN` set takes that pinned value and ignores the file, so
the pin wins here too. An instance that is not running is refused with the
`kallipctl start` command that would fix it: nothing here starts anything for
you.

A write may take a while: a concurrent writer's turn, the platform's
enrollment, and a displaced relay each have their own bound, so the client
waits up to 120 seconds on each call to the instance (an `add-entry` that
mints makes two: the check, then the write). When it gives up anyway, or
cannot read the answer, it says the instance may still be finishing, and
points at `polis entries SLUG` before you retry; do not mint another code
first, since the one in flight may already be spent.

An instance whose entries come from its environment (a spawn that carried
`--polis`) lists them and refuses both writes.

## Shell completions

Completion is dynamic, and there are two kinds. Instance slugs come from the
daemon as you type; platform names come from the polis register file, which
needs no daemon. Both degrade to no candidates rather than block typing when
their source is unavailable:

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
