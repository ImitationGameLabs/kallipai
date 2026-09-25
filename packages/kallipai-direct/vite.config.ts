import path from "node:path";
import { paraglideVitePlugin } from "@inlang/paraglide-js";
import tailwindcss from "@tailwindcss/vite";
import adapter from "@sveltejs/adapter-static";
import { sveltekit } from "@sveltejs/kit/vite";
import { defineConfig } from "vite";

// vite.config.ts lives in this package; the shared UI source is a sibling.
const here = import.meta.dirname;

export default defineConfig({
  plugins: [
    tailwindcss(),
    // i18n: compiles the shared inlang project (kallipai-ui/i18n) into
    // kallipai-ui/src/paraglide on dev/build start. project/outdir resolve
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
      project: "../kallipai-ui/i18n/project.inlang",
      outdir: "../kallipai-ui/src/paraglide",
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
      // SPA mode: a single index.html fallback shell boots the client-side app
      // (no SSR/Node runtime). Kept inline to match the existing
      // vite.config.ts convention rather than a separate svelte.config.js.
      adapter: adapter({
        fallback: "index.html",
      }),
    }),
    // kallipai-ui is consumed as live source from a sibling workspace package,
    // outside this package's watch root. Explicitly add it to the dev watcher so
    // edits there hot-reload instead of requiring a server restart.
    {
      name: "watch-kallipai-ui-source",
      configureServer(server) {
        server.watcher.add(path.resolve(here, "../kallipai-ui/src"));
      },
    },
  ],
  server: {
    // LAN-open so the operator can reach the fallback shell from other
    // devices on the network (by IP; no vhost, so no allowHosts list).
    // strictPort so a silent port drift can't be mistaken for this app.
    host: "0.0.0.0",
    port: 5175,
    strictPort: true,
    // The kallipai-ui live source (a sibling workspace package) lives outside this
    // package's root; allow the repo root so it serves without per-path
    // carve-outs.
    fs: {
      allow: [path.resolve(here, "../..")],
    },
  },
});
