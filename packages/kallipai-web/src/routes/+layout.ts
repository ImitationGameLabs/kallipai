// SPA mode: render client-side only. The adapter-static fallback (index.html)
// is the single emitted shell; routes are resolved in the browser. This keeps
// the build Tauri-ready (no SSR/Node runtime required).
export const ssr = false;
export const prerender = false;
