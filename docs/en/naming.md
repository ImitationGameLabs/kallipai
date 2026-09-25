---
title: Naming
description: Where the name kallipai comes from and the rules behind it.
order: 70
internal: true
---

This document records where the project's name comes from and the rules that
govern where each written form is used. For the rule that governs every name
use in the repository, see the
[Naming](https://github.com/ImitationGameLabs/kallipai/blob/main/AGENTS.md#naming) section of `AGENTS.md`.

## kallipai

The name comes from **kallipolis** (Greek _kalon_ + _polis_, "beautiful city"),
Plato's ideal city in the _Republic_. Kallipolis is, by design, an efficient
and harmonious structure of cooperating parts - each role doing its precise
work, the whole city functioning as a single well-ordered organism. That is
the picture we hold in mind for multi-agent coordination.

The name is built from the city's simplified stem **kallip** - shorter, easier
to type, free of the `-olis` that does no technical work - with **AI** as a
suffix that makes the project's nature unambiguous. Read together, `kallipai`
is literally `kallip` + `ai`.

### Service Names: Archeion and Polis

The platform's service names follow the same Greek-city framing. **archeion**
(ἀρχεῖον) was the ancient record office - the building that kept the citizen
registers and public archives. The identity service (enrollment, sessions,
tagma records) holds that job. **polis** (city) names the deployment
composition, which assembles the whole platform rather than any one service.

Four wire-protocol tags keep the `agora` spelling on purpose
(`kallipai-agora-aead-v1`, `kallipai-agora-enroll-v1`,
`kallipai-agora-tunnel-proof-v1`, `kallipai-agora-kex-v1`): they are
domain-separation strings that client SDKs match byte-for-byte and must never
be re-versioned.

### One Stem, Two Grandfathered Names

The stem governs every technical surface - crate names, binaries, Rust module
paths, env var prefixes (`KALLIPAI_*`), on-disk paths, container paths and
volumes, Nix attrs, Cargo/flake `description` strings, and the User-Agent: on
each of them, write `kallipai`.

Two names are grandfathered: the headless CLI **kallip** and the operator CLI
**kallipctl** keep their names - the commands users type, their reference
pages (`docs/*/reference/kallip.md`, `docs/*/reference/kallipctl.md`), and the
`crates/kallip` crate behind the CLI.
Everything else reads `kallipai`.

The rule exists to stop drift: a brand name that leaks into identifiers
becomes a renaming cost later, and a technical stem in prose makes the project
sound like a CLI flag.

The brand has **one written form**: **KallipAI** - used on every human-facing
surface: the README and doc H1, the site wordmark, prose brand mentions. The
lowercase identifier `kallipai` covers the repository name, the `@kallipai`
package scope, URLs, and domains. The recommended pronunciation is
**kallipai** (/ˈkælɪpaɪ/), regardless of how the brand is written. Neither
the brand form nor the identifier is a CLI name.
