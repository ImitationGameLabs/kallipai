/**
 * Service URL derivation for the web app, in two layers:
 *
 * 1. Explicit override — `config.apiBase` when set (non-empty): the
 *    platform edge origin (e.g. "https://api.example.com"), the same
 * 2. Derivation — the api.<domain> origin, following the page's own
 *    protocol and port: an https page on the default port reaches
 *    `https://api.example.com/v1/<service>`, while a page on a non-default
 *    port (the dev edge on :8080, say) reaches
 *    `http://api.example.com:8080/v1/<service>` — the edge listens there,
 *    so the api face carries it too.
 *
 * The deployment domain is the config's `domain` when set (taken verbatim,
 * `app.` prefix included), otherwise the page's own hostname with the
 * `app.` prefix stripped — the app is served at `app.<domain>`, and its
 * origin names the deployment. The `/v1` version segment and the service
 * segment are appended here so every client receives a base that already
 * carries its service (all client path templates are pure tails).
 */

export type ServiceName = "archeion" | "lesche" | "files" | "instances";

/** The subset of `window.KALLIP_CONFIG` this derivation consumes. */
export interface DerivationConfig {
  domain?: string;
  /** Platform-edge-origin override; empty string counts as unset. */
  apiBase?: string;
}

/** The parts of the browser location the derivation reads. */
export interface PageLocation {
  protocol: string;
  hostname: string;
  port?: string;
}

/** Derive the API base for one backend service from the config and page. */
export function serviceUrl(
  name: ServiceName,
  config: DerivationConfig,
  page: PageLocation,
): string {
  const override = config.apiBase;
  if (override !== undefined && override !== "") {
    return `${override.replace(/\/+$/, "")}/v1/${name}`;
  }
  const domain = config.domain ?? page.hostname.replace(/^app\./, "");
  const defaultPort = page.protocol === "https:" ? "443" : "80";
  const port = page.port && page.port !== defaultPort ? `:${page.port}` : "";
  return `${page.protocol}//api.${domain}${port}/v1/${name}`;
}
