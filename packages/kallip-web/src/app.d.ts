// See https://svelte.dev/docs/kit/types#app.d.ts
// for information about these interfaces
declare global {
  namespace App {
    // interface Error {}
    // interface Locals {}
    // interface PageData {}
    // interface PageState {}
    // interface Platform {}
  }
  interface Window {
    /**
     * Runtime deployment config, loaded from /config.js before the app
     * bundle (see static/config.js for the factory default; the NixOS
     * module bakes over it from runtimeConfig). Every field is
     * optional, and each layers differently: apiBase replaces the
     * whole derived API origin; domain overrides the deployment
     * domain used in that derivation; offlineLogin
     * defaults to true when unset (the factory file ships it as true).
     */
    KALLIP_CONFIG?: {
      /** Deployment domain (taken verbatim, `app.` prefix included). */
      domain?: string;
      /** True = show the operator-key login branch. */
      offlineLogin?: boolean;
      /** Whole-API-origin override; replaces the derived api.<domain>. */
      apiBase?: string;
    };
  }
}

export {};
