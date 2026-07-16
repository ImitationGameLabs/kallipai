import { svelte } from "@sveltejs/vite-plugin-svelte";
import { defineConfig } from "vite";

// Editor / typecheck config. kallip-ui is consumed as source by kallip-web, so
// it has no build step of its own; this config exists for tooling parity.
export default defineConfig({
  plugins: [svelte()],
});
