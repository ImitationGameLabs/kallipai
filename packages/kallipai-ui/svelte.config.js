import { vitePreprocess } from "@sveltejs/vite-plugin-svelte";

// kallipai-ui is a Svelte component library (not a SvelteKit app), so it carries
// only the preprocess config; a consuming app (kallipai-web, kallipai-app) provides
// SvelteKit.
export default {
  preprocess: vitePreprocess(),
};
