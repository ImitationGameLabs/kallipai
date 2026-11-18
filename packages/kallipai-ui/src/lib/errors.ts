// Headless error classification. Projects a raw thrown value (TransportError,
// KallipaiError, or anything else) into the { title, hint } a user-facing
// banner can render: only this package's own copy reaches the user, so a raw
// transport sentence (whatever route it names) never does. The full error
// (with cause chain) is logged separately to the browser console by the caller.
// Raw messages stay in the console: a server sentence or an arbitrary Error's
// .message is diagnostic, and the view carries only copy of our own.
//
// Pure, no runes, no app imports — stays reusable across consuming apps (the
// paraglide messages it renders are this package's own compiled output).

import {
  DataPlaneApiError,
  KallipaiError,
  TransportError,
} from "@kallipai/kallipai-common";
import {
  chat_send_failed,
  error_conversation_not_connected,
  error_couldnt_reach,
  error_hint_check_settings,
  error_request_rejected,
  error_tagma,
  error_unknown,
} from "../paraglide/messages.js";
import { isWriteRefusal } from "./session/writeRefusal.ts";

export interface ErrorView {
  readonly title: string;
  readonly hint?: string;
}

// instanceof can break if a bundler ever duplicates @kallipai/kallipai-common;
// both classes set `.name`, so accept by name as a fallback.
function isTransportError(e: unknown): e is TransportError {
  return (
    e instanceof TransportError ||
    (e instanceof Error && e.name === "TransportError")
  );
}

function isKallipaiError(e: unknown): e is KallipaiError {
  return (
    e instanceof KallipaiError ||
    (e instanceof Error && e.name === "KallipaiError")
  );
}

export function classifyError(e: unknown): ErrorView {
  // A refused write is ours, not the transport's, so it has copy of its own.
  if (isWriteRefusal(e)) {
    return { title: error_conversation_not_connected() };
  }
  if (isKallipaiError(e)) {
    // Guard `.api`: the name-based fallback above can match an Error renamed to
    // "KallipaiError" that lacks the `api` field, and this runs in a $derived, so
    // a throw here would blank the layout.
    const status = e.api?.status;
    // 4xx -> the request itself was rejected (user-actionable); 5xx -> the
    // tagma failed server-side.
    return {
      title:
        typeof status === "number" && status >= 500
          ? error_tagma()
          : error_request_rejected(),
    };
  }
  if (isTransportError(e)) {
    return {
      title: error_couldnt_reach(),
      hint: error_hint_check_settings(),
    };
  }
  return {
    title: error_unknown(),
  };
}

/** The copy a failed line carries, plus the failure's machine code when it
 *  named one (the chat view renders a code-specific sentence for the codes
 *  it knows). Only our own sentence reaches the line: the server's message
 *  and the raw error stay with the caller, which logs them, so a bilingual
 *  interface never shows a single-locale sentence by accident. */
export function sendFailureCopy(e: unknown): { copy: string; code?: string } {
  return { copy: chat_send_failed(), code: failureCode(e) };
}

/** The machine-readable reason, when the failure carried one: our error
 *  classes name it, a relay rejection frame carries it as a plain field,
 *  and anything else has none. */
function failureCode(e: unknown): string | undefined {
  if (isKallipaiError(e)) return e.api?.code;
  if (isDataPlaneApiError(e)) return e.code;
  if (typeof e === "object" && e !== null) {
    const code = (e as { code?: unknown }).code;
    if (typeof code === "string") return code;
  }
  return undefined;
}

function isDataPlaneApiError(e: unknown): e is DataPlaneApiError {
  return (
    e instanceof DataPlaneApiError ||
    (e instanceof Error && e.name === "DataPlaneApiError")
  );
}
