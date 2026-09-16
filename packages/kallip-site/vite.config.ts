import tailwindcss from "@tailwindcss/vite";
import adapter from "@sveltejs/adapter-static";
import { sveltekit } from "@sveltejs/kit/vite";
import { defineConfig } from "vite";

// The marketing/docs site is a fully prerendered static package: every route
// renders at build time (prerender = true in the root layout), so the adapter
// emits plain HTML with no SPA fallback. Kit conventions (adapter, runes) stay
// inline here, matching the kallip-web package's vite.config.ts convention.
export default defineConfig({
  plugins: [
    tailwindcss(),
    sveltekit({
      compilerOptions: {
        runes: ({ filename }) =>
          filename.split(/[/\\]/).includes("node_modules") ? undefined : true,
      },
      adapter: adapter(),
    }),
  ],
});
