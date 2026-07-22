import type { TagmaClientOptions } from "./client.ts";

/**
 * Read tagma connection options from the environment. Deno/Node only: intended
 * for local scripts and tests. The browser bundle must NOT call this (browsers
 * have no environment), which is why it lives behind the `./env` subpath export
 * rather than the package main entry.
 */
export function tagmaClientOptionsFromEnv(): TagmaClientOptions {
  const baseUrl = Deno.env.get("KALLIP_TAGMA_URL") ?? "http://127.0.0.1:3000";
  const authToken = Deno.env.get("KALLIP_AUTH_TOKEN");
  if (!authToken) {
    throw new Error(
      "KALLIP_AUTH_TOKEN is required; set it to a tagma operator token",
    );
  }
  return { baseUrl, authToken };
}
