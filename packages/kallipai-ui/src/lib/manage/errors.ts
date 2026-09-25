// Discriminate typed server errors from raw failures for store error state.
//
// KallipaiError carries the server envelope message (safe to display as-is);
// anything else (transport, decode, local assembly) is logged with the scope
// tag and replaced by the caller's qualitative catalog fallback.
import { KallipaiError } from "@kallipai/kallipai-common";

export function displayError(
  scope: string,
  e: unknown,
  fallback: string,
): string {
  if (e instanceof KallipaiError) return e.message;
  console.error(`[${scope}] request failed:`, e);
  return fallback;
}
