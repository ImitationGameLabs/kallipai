import path from "node:path";
import tailwindcss from "@tailwindcss/vite";
import adapter from "@sveltejs/adapter-static";
import { sveltekit } from "@sveltejs/kit/vite";
import { defineConfig } from "vite";

// Set by `tauri android dev`/`tauri ios dev` to the host IP the device/emulator
// must reach. When present we bind externally and repoint HMR so the mobile
// WebView can hot-reload; otherwise Vite binds localhost (desktop).
const host = process.env.TAURI_DEV_HOST;

// vite.config.ts lives in this package; the shared UI source is a sibling.
const here = import.meta.dirname;

export default defineConfig({
  plugins: [
    tailwindcss(),
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

  // Keep the dev server in lockstep with src-tauri/tauri.conf.json's devUrl.
  // strictPort: a busy port must error, not silently drift off devUrl.
  // watch: ignore the Rust src-tauri tree so editing it doesn't restart Vite.
  clearScreen: false,
  server: {
    port: 8080,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: "ws", host, port: 8081 } : undefined,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
});
