# Frontend package development

This guide covers the JS/TS workspace packages under `packages/` (`kallip-common`,
`kallip-client`, `kallip-agora-client`, `kallip-lesche-client`, `kallip-ui`,
`kallip-web`, `kallip-app`). They share a single toolchain, **Deno**, laid out as
an npm-style workspace but never driven by npm. The Rust crates under `crates/`
are unrelated (cargo).

The frontend reaches the tagma over two transports behind one `Transport` seam
(in `kallip-ui/src/lib/session/`): `DirectTransport` for the offline path
(consuming `/agents/{id}/external/events` via `kallip-client`) and a
`RelayChannel` from `kallip-lesche-client` for the online E2EE path. Both reduce
through the shared `kallip-ui/src/lib/transcript.ts` reducer
(`applyTagmaReply` / `applySignal`).

The short version: **everything goes through `deno task`. Never drop down to
`npm` / `npx` / `pnpm` / `yarn`, and never hand-invoke `node_modules/.bin/*`.**

## Why Deno only

- `deno.lock` is the source of truth for versions. Another installer (`npm i`,
  `pnpm install`, …) rewrites lockfiles and drifts the tree.
- The root `package.json` `scripts` and each package's `scripts` are all written
  as Deno tasks; `engines.deno` is `>=2.9.2`.
- Calling a binary straight from `node_modules/.bin/*` also bypasses the
  per-package Prettier config (see below), so formatting silently drops plugins.

## Prerequisites

Deno `>= 2.9.2`. Deno 2 reads each `package.json`'s `scripts` as tasks, so
`deno task <name>` works in any package directory and at the repo root.

## Tasks

Run tasks **from the repo root**. Workspace filtering and plugin resolution
assume it.

| Task                        | What it does                                                            |
| --------------------------- | ----------------------------------------------------------------------- |
| `deno task dev`             | Dev server for `kallip-web`                                             |
| `deno task build`           | Build `kallip-web`                                                      |
| `deno task check`           | Type / svelte checks across all JS/TS packages                          |
| `deno task test`            | Run tests for the packages that define them                             |
| `deno task sync`            | `svelte-kit sync` for `kallip-web`                                      |
| `deno task fmt`             | Prettier (TS/Svelte/CSS/JSON) + rumdl (Markdown) across the repo        |
| `deno task fmt:file <path>` | Prettier `--write` a single non-md file (format just what you touched)  |
| `deno task fmt:md <path>`   | rumdl `fmt` a single Markdown file                                      |
| `deno task fmt:check`       | Prettier `--check .` + rumdl `fmt --check .` (CI-style, no writes)      |
| `deno task lint`            | `deno lint`                                                             |

### Single package (run inside the package directory)

Each package's own `scripts` are available as `deno task <name>` — e.g. inside
`packages/kallip-web`: `deno task dev`, `deno task check`, `deno task build`,
`deno task prepare`. Use these for a tight edit loop on one package; use the root
tasks when a change spans packages.

## Tauri Android app (`kallip-app`)

`kallip-app` is the Tauri Android target (desktop is intentionally not built;
use `kallip-web` in a browser). Its SvelteKit frontend is a normal package, but
the Tauri/Android toolchain is **not** in the default devShell, so enter the
mobile shell first:

```sh
nix develop .#tauri            # or set KALLIP_DEVSHELL=tauri under direnv
```

Then, from `packages/kallip-app`:

```sh
deno task tauri android dev                           # emulator, defaults to x86_64
deno task tauri android build --target aarch64        # real device (arm64-v8a)
deno task tauri android build --target x86_64         # emulator
```

The `tauri android init` output (under `src-tauri/gen/`) is committed, so you
normally skip `init` entirely. Re-run `deno task tauri android init
--skip-targets-install` only to regenerate that tree.

`build` requires `--target`; `dev` does not. The rust-overlay toolchain ships
the cross std targets directly and has no `rustup`, so `build` (unlike `dev`)
re-shells out to `rustup target add` and fails unless the target is named
explicitly. The full toolchain rationale lives in
[nix/devshells/tauri.nix](../nix/devshells/tauri.nix).

## Formatting

Two formatters, split by file type:

- **Prettier** owns TS / Svelte / CSS / JSON. Format with `deno task fmt:file
  <path>` (one file) or `deno task fmt` (whole tree). Both load plugins correctly
  because Prettier is launched the Deno way, from the repo root. Prettier comes
  from the root `devDependencies` (self-contained in `node_modules`), not the
  nix devshell.
- **rumdl** owns Markdown. Format with `deno task fmt:md <path>` (one file) or
  `deno task fmt` (whole tree). rumdl is provided by the nix devshell. Markdown
  is in `.prettierignore` so Prettier never touches it: Prettier pads/aligns
  table columns, which reflows every row on any one-line edit; rumdl does not.
  rumdl is configured via `.rumdl.toml` (disables line-length and the
  `$ `-prompt rule).

Prettier plugins are **declared per package** in a local `.prettierrc.json`,
scoped to where they are actually used:

- `packages/kallip-web`, `packages/kallip-app` — Tailwind + Svelte:
  `["prettier-plugin-tailwindcss", "prettier-plugin-svelte"]`
- `packages/kallip-ui` — Svelte only: `["prettier-plugin-svelte"]`
- `kallip-common`, `kallip-client`, `kallip-agora-client` — plain TS, no plugins.

The plugin npm packages themselves are root `devDependencies` (shared formatter
tooling); the per-package config only declares which plugins apply where.

> **Load order matters.** `prettier-plugin-tailwindcss` MUST be listed **before**
> `prettier-plugin-svelte`. Reversed order crashes every `.svelte` format with
> `getVisitorKeys is not a function or its return value is not iterable` (a known
> interaction between the two plugins). Keep the order above.

`.prettierignore` excludes `node_modules`, `.svelte-kit`, `build`, `dist`,
`deno.lock`, `target`, `crates`, and `**/*.md` (rumdl's domain).

## Looking up package versions

Deno has no equivalent of `npm view` / `npm search` for the npm registry
(`deno search` is JSR-only). Read registry metadata via its HTTP API:

```sh
curl -s https://registry.npmjs.org/<package>/latest
```

Or just set a `^` range in `package.json`, run `deno install`, and read the
resolved version from its output or `deno.lock`. Do not use `npm view` /
`npm info`.

## Adding a dependency

1. Add the package and version to the target package's `package.json`
   (`dependencies` or `devDependencies`).
2. From the repo root, run `deno install` to update `deno.lock` and
   `node_modules`.

`deno add npm:<pkg>` does both steps at once -- the `npm:` prefix is
mandatory (without it Deno resolves JSR and writes to `deno.json`); add
`-D` for devDependencies. Do not use `npm install` / `npm i`.

## If a workflow isn't covered

Add a script to the root `package.json` `scripts` (or the relevant package's)
and call it through `deno task`. This keeps the toolchain uniform and
discoverable for every agent, instead of each one hand-rolling a
`node_modules/.bin/...` invocation.

## Before committing changes in `packages/`

- `deno task check` — types / svelte checks for the touched package(s).
- `deno task fmt:file <paths>` (or `deno task fmt`) — formatting.
- `deno task build` — for packages with a build step (e.g. `kallip-web`).
- `deno task lint` — Deno lint.
