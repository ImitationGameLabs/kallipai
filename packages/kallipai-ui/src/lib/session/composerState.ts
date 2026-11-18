// What a chat page's composer should say once the write axis is gone, and
// whether it can offer a way back.
//
// Pure on purpose: no runes, no i18n. The page maps the returned notice to a
// message, so the same sentence covers whichever reason left the axis dead --
// a closed conversation and an absent carrier both just mean "not connected".

import type { LinkStatus } from "./conversation/shared.ts";

/** What the composer says: the handshake first, then silence while the axis
 *  writes, else the notice that it is gone. */
export type ComposerNotice = "opening" | "not-connected";

export interface ComposerState {
  readonly notice: ComposerNotice | null;
  readonly retry: boolean;
}

/** Only `retry` reads the kind: a relay channel is the one carrier the store
 *  can be asked to reopen, while a direct sender and the offline view come
 *  back on their own. The notice reads the write axis alone. */
export function composerState(c: {
  kind: "local" | "relay" | "offline-view";
  status: LinkStatus;
  canWrite: boolean;
}): ComposerState {
  if (c.status === "opening") return { notice: "opening", retry: false };
  if (c.canWrite) return { notice: null, retry: false };
  return { notice: "not-connected", retry: c.kind === "relay" };
}

/** How the composer's note area treats what it was handed: say the string,
 *  fall back to the pairing sentence, or stay empty. Three states, three
 *  exits, so no branch has to lean on the one around it. An empty string is
 *  said as itself: no page produces one, so it carries nothing to protect. */
export type NoticeRender = "say" | "fallback" | "none";

export function noticeRender(notice: string | null | undefined): NoticeRender {
  if (typeof notice === "string") return "say";
  return notice === undefined ? "fallback" : "none";
}
