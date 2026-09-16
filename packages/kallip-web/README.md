# kallip-web

The kallipai web app: a SvelteKit SPA (adapter-static) built on the shared
`kallip-ui` package. Part of the JS/TS workspace under `packages/`; see
`docs/en/development/frontend.md` for the toolchain — everything runs through
`deno task`, never npm/npx.

## Commands

From this directory (`deno task` merges the root tasks with this
package's scripts and resolves the closest match):

- `deno task dev` — vite dev server (:5173, the `app.` edge in the dev Caddyfile)
- `deno task build` — production build into `build/` (adapter-static SPA)
- `deno task check` — svelte-check
- `deno task sync` — resolves to the root sync task, which runs this
  package's prepare hook: svelte-kit sync plus a paraglide recompile

## i18n

Messages live in `../kallip-ui/i18n/project.inlang/messages/<locale>/*.json`.
After adding or changing keys, regenerate the paraglide output from the repo
root with `deno task i18n` (the compiled output is gitignored). The two inlang
plugins are pinned as dev dependencies here and referenced from the project
settings by their `node_modules` paths (resolved relative to the
project.inlang directory, so three levels up reaches the repo root).

## Deployment

kallip-web is the browser app — it owns the `app.<domain>` web origin.
[`packages/kallip-app`](../kallip-app) is the native release shape (Tauri
installers): no web origin, no subdomain — it bakes `KALLIP_POLIS_URL` at
build time and talks to `api.<domain>/v1/*` directly.

- **Dev**: the host vite dev server behind the dev Caddyfile (`app.<domain>`).
- **NixOS**: the flake's `packages.kallip-web-dist` builds the bundle (two
  derivations: a networked deps build and an offline vite build), and the
  module's `web.distWithRuntimeConfig` option derives the site root that
  the deployment's own edge serves. See `docs/en/reference/container.md`.
