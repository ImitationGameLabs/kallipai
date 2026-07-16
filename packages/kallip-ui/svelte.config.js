import { vitePreprocess } from "@sveltejs/vite-plugin-svelte";

// kallip-ui is a Svelte component library (not a SvelteKit app), so it carries
// only the preprocess config; a consuming app (kallip-web, kallip-app) provides
// SvelteKit.
export default {
  preprocess: vitePreprocess(),
};
