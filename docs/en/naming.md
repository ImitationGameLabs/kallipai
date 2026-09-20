---
title: Naming
description: The kallip/kallipai naming split and the rules behind it.
order: 70
internal: true
---

This document records how the project's name evolved and why each name was
chosen. It exists so the rationale behind `kallipai` / `kallip` is not lost,
and so future contributors can tell the brand surface from the technical stem.

For the rule that governs where each name is used today, see the
[Naming](https://github.com/ImitationGameLabs/kallipai/blob/main/AGENTS.md#naming) section of `AGENTS.md`.

## just-llm-libs

Before there was an agent, there was frustration. Across earlier agent work we
tried one LLM library and framework after another, and kept hitting the same
wall: too much wrapping, too much opinion, too much hidden machinery we could
not reach past. Every framework wanted to own the loop, the prompt, the
tool dispatch, the context. None of them wanted to just _be a library_.

So `just-llm-libs` was born out of refusal. The name says it directly: _just_
LLM libraries - pure, lightweight, composable. Pieces you combine yourself,
instead of a runtime that combines them for you and walls them off. The goal
was to keep the agent in control of every decision the framework had been
silently making on its behalf.

### just-agent

`just-agent` started life as a single example inside `just-llm-libs` - one way
to compose those libraries into an actual agent. It carried the same `just`
ethos: no more framework than necessary, no hidden behavior, nothing between
the author and the model.

It did not stay an example for long. As it grew, it stopped being a demo of the
libraries and started being the thing we actually wanted to build. The scope
widened from "an agent" to "an agent harness" - the tagma, the policy layer,
the clients, the multi-agent shape. At that point the `just-` prefix was no
longer doing descriptive work; it was the name of an upstream library, not a
product.

### kallipai

The rename marked a real shift in ambition: from a library example to a
production-grade agent harness, designed for multi-agent systems from the very
beginning. We wanted a name that could carry a brand - something formal, ownable,
and worth saying out loud - rather than a working title inherited from the code
that happened to spawn it.

The name comes from **kallipolis** (Greek _kalon_ + _polis_, "beautiful city"),
Plato's ideal city in the _Republic_. Kallipolis is, by design, an efficient
and harmonious structure of cooperating parts - each role doing its precise
work, the whole city functioning as a single well-ordered organism. That is
the picture we hold in mind for multi-agent coordination.

For day-to-day use we take the simplified stem **kallip** - shorter, easier to
type, free of the `-olis` that does no technical work - and add **AI** as a
suffix to make the project's nature unambiguous. Read together, `kallipai` is
literally `kallip` + `ai`.

#### Service Names: Archeion and Polis

The platform's service names follow the same Greek-city framing. **archeion**
(ἀρχεῖον) was the ancient record office - the building that kept the citizen
registers and public archives. The identity service (enrollment, sessions,
tagma records) held that job all along; its former name, **agora** (the
marketplace), described a role it never had. **polis** (city) names the
deployment composition, which assembles the whole platform rather than any
one service.

The 2026-09 rename retired `agora` from code, packages, deployment, and
environment variables (`KALLIP_AGORA_*` -> `KALLIP_ARCHEION_*`), removing the
collision with Agora.io along the way. Four wire-protocol tags keep the old
spelling on purpose (`kallip-agora-aead-v1` and siblings): they are
domain-separation strings that client SDKs match byte-for-byte and must never
be re-versioned.
Two whitelist notes for future residual scans:
`skills/code/commit-messages.md` keeps one `agora` in a fictional
message-shape example (a mirror of the upstream skills library - fix
it there, not here), and this section itself quotes the old names as
historical record.

| Before | After |
| --- | --- |
| `kallip-agora` / `-common` / `-client` (crates and package) | `kallip-archeion` / `-common` / `-client` |
| `compose/{dev,prod}/agora.nix` | `compose/{dev,prod}/polis.nix` |
| `services.agora` / `agora-postgres` | `services.archeion` / `archeion-postgres` |
| `KALLIP_AGORA_*` / `KALLIP_*_AGORA_*` | `KALLIP_ARCHEION_*` / `KALLIP_*_ARCHEION_*` |
| `agora.<domain>` / `agora.kallipai.lan` | `archeion.<domain>` / `archeion.kallipai.lan` |
| `agora_pgdata` volume | `archeion_pgdata` (migrate old data manually) |
| `--agora-url` / `--relay-agora-url` CLI flags | `--archeion-url` / `--relay-archeion-url` |
| `agora.url` credentials file | `archeion.url` (absent old file backfills) |

#### The Two-Stem Rule

The name is split into **two stems** that serve different audiences. The split
is deliberate and is enforced in `AGENTS.md`:

- **Brand stem** - human-facing. The README and doc H1, the opening identity
  sentence of a doc, prose where the project speaks as a product.
- **Technical stem** - `kallip`, everywhere a machine or an identifier reads
  it: crate names, binaries, Rust module paths, env var prefixes (`KALLIP_*`),
  on-disk paths, container paths and volumes, Nix attrs, Cargo/flake
  `description` strings, the User-Agent, the Harbor integration's `name()`.

The rule exists to stop drift: a brand name that leaks into identifiers
becomes a renaming cost later, and a technical stem in prose makes the project
sound like a CLI flag. Keeping the two stems separate keeps both readable.

The brand stem has **one written form**: **KallipAI** - used on every
human-facing surface: the README and doc H1, the site wordmark, prose brand
mentions.

The operator consolidated the brand's written forms on 2026-09-15: until
then the stem had two written forms, the lowercase identifier `kallipai`
and the two-word `Kallip AI`, and prose picked whichever read better; a
single mark keeps the wordmark and doc titles consistent.

The lowercase token `kallipai` survives as an identifier only: repository,
package names, URLs, and domains. The recommended pronunciation is
**kallipai** (/ˈkælɪpaɪ/), regardless of how the brand is written. Neither
the brand form nor the identifier is the technical stem.
