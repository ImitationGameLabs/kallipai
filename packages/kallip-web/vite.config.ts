import path from "node:path";
import tailwindcss from "@tailwindcss/vite";
import adapter from "@sveltejs/adapter-static";
import { sveltekit } from "@sveltejs/kit/vite";
import { defineConfig } from "vite";

// vite.config.ts lives in this package; the shared UI source is a sibling.
const here = import.meta.dirname;

// The dev domain (see compose/dev/agora.nix `devDomain`). The same env var drives
// the agora/lesche env and the Caddyfile, so the whole stack agrees on one
// name; override it in `.env`. Default must match compose/dev/agora.nix
// and +layout.svelte.
const devDomain = process.env.KALLIP_DEV_DOMAIN ?? "kallipai.com";
const webHost = `web.${devDomain}`;

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
  // Re-export the resolved domain to the client as import.meta.env.KALLIP_DEV_DOMAIN
  // so +layout.svelte can derive the agora/lesche base URLs from the SAME value
  // (VITE_AGORA_URL / VITE_LESCHE_URL still win if set explicitly).
  define: {
    "import.meta.env.KALLIP_DEV_DOMAIN": JSON.stringify(devDomain),
  },
  server: {
    // The dev stack is fronted by Caddy, which terminates TLS for *.devDomain
    // and reverse-proxies web.<devDomain> to this dev server. Bind 0.0.0.0 so
    // the Caddy container (reaching the host via the host gateway) can connect;
    // a loopback-only bind would be unreachable from inside the compose network.
    host: true,
    port: 5173,
    strictPort: true,
    // Caddy forwards the incoming Host header unchanged, and vite is plain HTTP
    // here (so the HTTPS host-check exemption does NOT apply). Without an
    // explicit allow entry for the proxied hostname, vite rejects the request.
    allowedHosts: [webHost],
    // HMR rides the same https origin: the browser reconnects over wss on 443
    // (Caddy's face, which proxies the upgrade), while vite's own websocket
    // stays on 5173. clientPort is the browser-side override (using `port`
    // would instead make vite's WS server try to listen on 443).
    ws: {
      protocol: "wss",
      host: webHost,
      clientPort: 443,
    },
  },
});
