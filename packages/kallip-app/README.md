# kallip-app

The kallipai native app: the SvelteKit source that Tauri wraps into the
mobile/desktop installers. Part of the JS/TS workspace under `packages/`;
see `docs/en/development/frontend.md` for the toolchain — everything runs
through `deno task`, never npm/npx.

kallip-app is the **native release shape** of the web experience, not a
second website: it has no web origin and no subdomain. The backend base
is baked at build time from `KALLIP_POLIS_URL` (exposed to the bundle via
the vite `envPrefix`), and every client talks to `<origin>/v1/<service>`
directly. The browser deployment of the same UI is
[`packages/kallip-web`](../kallip-web) — it owns the `app.<domain>` web
origin.

## Commands

From this directory (`deno task` merges the root tasks with this
package's scripts and resolves the closest match):

- `deno task dev` — vite dev server for the Tauri webview
- `deno task build` — production build (consumed by the Tauri packaging)
- `deno task check` — svelte-check
- `deno task tauri` — the Tauri CLI (desktop dev/build flows)

## i18n

Messages live in `../kallip-ui/i18n/project.inlang/messages/<locale>/*.json`.
After adding or changing keys, regenerate the paraglide output from the repo
root with `deno task i18n` (the compiled output is gitignored).
