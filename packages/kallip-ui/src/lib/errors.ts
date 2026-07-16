// Headless error classification. Projects a raw thrown value (TransportError,
// KallipError, or anything else) into the { title, detail, hint } a user-facing
// banner can render, so we never leak internal paths like
// `daemon request failed: /agents/<id>/events` to the user. The full error
// (with cause chain) is logged separately to the browser console by the caller.
//
// Pure, no runes, no app imports — stays reusable across consuming apps.

import { KallipError, TransportError } from "@kallipai/kallip-common";

export interface ErrorView {
  readonly title: string;
  readonly detail?: string;
  readonly hint?: string;
}

// instanceof can break if a bundler ever duplicates @kallipai/kallip-common;
// both classes set `.name`, so accept by name as a fallback.
function isTransportError(e: unknown): e is TransportError {
  return (
    e instanceof TransportError ||
    (e instanceof Error && e.name === "TransportError")
  );
}

function isKallipError(e: unknown): e is KallipError {
  return (
    e instanceof KallipError || (e instanceof Error && e.name === "KallipError")
  );
}

export function classifyError(e: unknown): ErrorView {
  if (isKallipError(e)) {
    // Guard `.api`: the name-based fallback above can match an Error renamed to
    // "KallipError" that lacks the `api` field, and this runs in a $derived, so
    // a throw here would blank the layout.
    const status = e.api?.status;
    // 4xx -> the request itself was rejected (user-actionable); 5xx -> the
    // daemon failed server-side.
    return {
      title:
        typeof status === "number" && status >= 500
          ? "Daemon error"
          : "Request rejected",
      detail: e.api?.message,
    };
  }
  if (isTransportError(e)) {
    return {
      title: "Couldn't reach the daemon",
      hint: "Check that the daemon is running and the URL in Settings is correct.",
    };
  }
  return {
    title: "Something went wrong",
    detail: e instanceof Error ? e.message : undefined,
  };
}
