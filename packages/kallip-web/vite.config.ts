import path from "node:path";
import { paraglideVitePlugin } from "@inlang/paraglide-js";
import tailwindcss from "@tailwindcss/vite";
import adapter from "@sveltejs/adapter-static";
import { sveltekit } from "@sveltejs/kit/vite";
import { defineConfig } from "vite";

// vite.config.ts lives in this package; the shared UI source is a sibling.
const here = import.meta.dirname;

// Dev-server shaping only (allowedHosts/HMR ws): the KALLIP_* env is no
// longer baked into the client bundle — service URLs derive at runtime
// in +layout.svelte from the browser location (config.js overrides).
// The dev stack is fronted by the compose caddy edge (always on), and
// these bindings name that edge so `deno task dev` points at the same
// entry the containers do; they live in `.env` (direnv puts it in the
// shell, and vite reads process.env from there).
const devDomain = process.env.KALLIP_DOMAIN ?? "kallipai.com";
const edgeTls = (process.env.KALLIP_EDGE_TLS ?? "on") === "on";
const edgePort = Number(process.env.KALLIP_EDGE_PORT ?? "443");
const appHost = `app.${devDomain}`;

export default defineConfig({
  plugins: [
    tailwindcss(),
    // i18n: compiles the shared inlang project (kallip-ui/i18n) into
    // kallip-ui/src/paraglide on dev/build start. project/outdir resolve
    // against this package's cwd; the project's pathPattern resolves
    // against the project directory's parent (@inlang/sdk behavior).
    // Pinned to 2.20.0: on 2.24.x the compile silently produced zero
    // messages while reporting success (root cause not identified).
    // outputStructure is pinned so dev, build, and the root `npm run i18n`
    // (CLI default) all emit the same layout — the plugin default is
    // locale-modules in dev, which would flip the shared outdir layout
    // between dev and build.
    // Keep exactly one compile trigger running per app.
    paraglideVitePlugin({
      project: "../kallip-ui/i18n/project.inlang",
      outdir: "../kallip-ui/src/paraglide",
      strategy: ["cookie", "preferredLanguage", "baseLocale"],
      emitTsDeclarations: true,
      outputStructure: "message-modules",
    }),
    sveltekit({
      compilerOptions: {
        // Force runes mode for the project, except for libraries. Can be removed in svelte 6.
        runes: ({ filename }) =>
          filename.split(/[/\\]/).includes("node_modules") ? undefined : true,
      },
      // SPA mode: a single index.html fallback shell boots the client-side app,
      // Tauri-ready (no SSR/Node runtime). Kept inline to match the existing
      // vite.config.ts convention rather than a separate svelte.config.js.
      adapter: adapter({
        fallback: "index.html",
      }),
    }),
    // kallip-ui is consumed as live source from a sibling workspace package,
    // outside this package's watch root. Explicitly add it to the dev watcher so
    // edits there hot-reload instead of requiring a server restart.
    {
      name: "watch-kallip-ui-source",
      configureServer(server) {
        server.watcher.add(path.resolve(here, "../kallip-ui/src"));
      },
    },
  ],
  server: {
    // The dev stack is fronted by Caddy, which terminates TLS for *.devDomain
    // and reverse-proxies app.<devDomain> to this dev server. Bind 0.0.0.0 so
    // the Caddy container (reaching the host via the host gateway) can connect;
    // a loopback-only bind would be unreachable from inside the compose network.
    host: true,
    port: 5173,
    strictPort: true,
    // The kallip-ui live source (a sibling workspace package) lives outside this
    // package's root; allow the repo root so it serves without per-path
    // carve-outs.
    fs: {
      allow: [path.resolve(here, "../..")],
    },
    // Caddy forwards the incoming Host header unchanged, and vite is plain HTTP
    // here (so the HTTPS host-check exemption does NOT apply). Without an
    // explicit allow entry for the proxied hostname, vite rejects the request.
    allowedHosts: [appHost],
    // HMR rides the edge: the browser reconnects to app.<devDomain> on
    // the edge port (wss behind the https edge, ws behind the plain one;
    // caddy proxies the upgrade either way), while vite's own websocket
    // stays on 5173.
    ...(edgeTls
      ? {
          ws: { protocol: "wss" as const, host: appHost, clientPort: edgePort },
        }
      : { ws: { host: appHost, clientPort: edgePort } }),
  },
});
