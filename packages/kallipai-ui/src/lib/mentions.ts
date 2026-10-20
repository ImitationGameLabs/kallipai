// The room composer's mention picker, as pure rules: where an `@` token starts,
// which members it offers, what a key press means, and how a pick joins the
// list. The component owns no decisions -- it holds three integers and calls
// these -- because the keyboard rules and the token boundary are exactly the
// parts a screenshot cannot check and a typecheck cannot see.
//
// The vocabulary is the room's own: a candidate is a `MentionTarget` (the
// roster row the mention chips already resolve against), an id is a
// participant id, and the cap is `MENTIONS_MAX` -- the same 16 the wire
// enforces, imported from the codec rather than repeated here.

import { type MentionTarget, MENTIONS_MAX } from "./room-message.ts";

/** The DOM id of the popup's list, and of one candidate row, in one place:
 *  the field names both (`aria-controls`, `aria-activedescendant`) and the
 *  popup renders them, so a drift between the two would be silent. */
export const MENTION_LIST_ID = "composer-mention-list";

export function mentionOptionId(id: string): string {
  return `composer-mention-option-${id}`;
}

/** How many candidates the popup shows at once. A bounded list keeps arrow-key
 *  navigation meaningful without scrolling the selection into view: a longer
 *  roster is narrowed by typing, not by paging. */
export const MENTION_PICKER_LIMIT = 8;

/** The `@` token the caret sits in. `start` is the `@` itself, `end` is one past
 *  the last character of the word, so accepting replaces the whole token even
 *  when the caret stopped in its middle. */
export interface MentionToken {
  readonly start: number;
  readonly end: number;
  /** The text between `@` and the end of the word, as typed. */
  readonly query: string;
}

/** The characters that keep an `@` from opening a picker: an ASCII identifier
 *  run. Everything else opens -- whitespace, punctuation, and any non-ASCII
 *  character, so `你好@ada` works the way Chinese prose is actually written.
 *  Full-width letters and digits count as non-identifiers for the same reason:
 *  they do not form a handle, so nothing is lost by opening. */
const IDENTIFIER_CHAR = /[A-Za-z0-9_]/;

const WHITESPACE = /\s/;

/** The mention token under the caret, or undefined when the caret is not in
 *  one. `draft` is the whole text; `caret` is the textarea's selectionStart.
 *
 *  A token needs: an `@` before the caret, a previous character that is not an
 *  identifier character (`a@b` is an address, not a mention), a word that runs
 *  from the `@` to whitespace or the end of the draft, no second `@` inside it,
 *  and a caret no further than the end of that word (a caret past the word has
 *  left the token). */
export function mentionTokenAt(
  draft: string,
  caret: number,
): MentionToken | undefined {
  if (!Number.isInteger(caret) || caret < 0 || caret > draft.length) {
    return undefined;
  }
  const start = draft.lastIndexOf("@", caret - 1);
  if (start < 0) return undefined;
  // The caret has to be past the `@` itself: `lastIndexOf` clamps a start of
  // -1 to 0, so a caret at 0 would otherwise read `@ad` as a token.
  if (caret <= start) return undefined;
  if (start > 0 && IDENTIFIER_CHAR.test(draft.slice(start - 1, start))) {
    return undefined;
  }
  let end = start + 1;
  while (end < draft.length && !WHITESPACE.test(draft.slice(end, end + 1))) {
    end += 1;
  }
  if (caret > end) return undefined;
  const query = draft.slice(start + 1, end);
  // A second `@` inside the word means the first one was not a token opener.
  if (query.includes("@")) return undefined;
  return { start, end, query };
}

/** The candidates a query offers, ordered: members whose label, handle or id
 *  starts with the query first, then those that merely contain it. Inside a
 *  bucket the roster's own order is kept, so the list is stable without
 *  depending on a locale collation. Capped at `limit`. */
export function rankMentionCandidates(
  members: readonly MentionTarget[],
  query: string,
  limit = MENTION_PICKER_LIMIT,
): MentionTarget[] {
  const needle = query.toLowerCase();
  const prefix: MentionTarget[] = [];
  const contains: MentionTarget[] = [];
  for (const member of members) {
    const fields = [member.label, member.handle, member.id]
      .filter((field): field is string => typeof field === "string")
      .map((field) => field.toLowerCase());
    if (fields.some((field) => field.startsWith(needle))) prefix.push(member);
    else if (fields.some((field) => field.includes(needle))) {
      contains.push(member);
    }
  }
  return [...prefix, ...contains].slice(0, limit);
}

/** What one key press means while the picker is open. */
export type MentionKeyIntent = "next" | "prev" | "accept" | "dismiss" | "none";

/** The key fields the decision reads; a KeyboardEvent satisfies it. */
export interface MentionKey {
  readonly key: string;
  readonly isComposing: boolean;
  readonly shiftKey?: boolean;
}

/** The picker's key rules, decided before the composer's own: an open popup
 *  consumes the navigation keys and the accepting ones, and a composing key is
 *  always the IME's (a candidate window's Enter confirms a character, it does
 *  not pick a member and does not submit). With nothing to accept, Enter and
 *  Tab fall through, so an `@` that matches nobody still sends normally. */
export function mentionKeyIntent(
  state: { readonly open: boolean; readonly candidates: number },
  event: MentionKey,
): MentionKeyIntent {
  if (!state.open) return "none";
  if (event.isComposing) return "none";
  switch (event.key) {
    case "ArrowDown":
      return "next";
    case "ArrowUp":
      return "prev";
    case "Enter":
      // Shift+Enter is the composer's line break at every breakpoint.
      return event.shiftKey === true || state.candidates === 0
        ? "none"
        : "accept";
    case "Tab":
      return state.candidates === 0 ? "none" : "accept";
    case "Escape":
      return "dismiss";
    default:
      return "none";
  }
}

/** Where a direction key lands in a list of `length` rows: a ring, so the
 *  last row's next is the first and the first row's prev is the last. An
 *  empty list has a single place to stand. */
export function stepMentionIndex(
  current: number,
  length: number,
  delta: number,
): number {
  if (length <= 0) return 0;
  return (((current + delta) % length) + length) % length;
}

/** The identity a dismissal and a cap refusal key on. Moving to another
 *  token -- or leaving tokens altogether -- yields a different key, so both
 *  expire by themselves instead of through a reset a caller could forget. */
export function mentionTokenKey(
  token: MentionToken | undefined,
): string | null {
  return token === undefined
    ? null
    : `${token.start}:${token.end}:${token.query}`;
}
/** What the popup derives from the state one keyboard session holds: open
 *  while the token offers a candidate and this very token was not dismissed;
 *  the cursor clamped into the rows, because a new query shortens the list;
 *  which row the field points `aria-activedescendant` at; and whether a cap
 *  refusal still belongs to this token. Pure, so the state that feeds it can
 *  arrive from a test shim and the rules still run over it. */
export interface MentionPopupView {
  readonly open: boolean;
  readonly activeIndex: number;
  readonly activeId: string | undefined;
  readonly limitHit: boolean;
}

export function mentionPopupView(input: {
  readonly token: MentionToken | undefined;
  readonly rows: readonly MentionTarget[];
  readonly cursor: number;
  readonly dismissed: string | null;
  readonly capped: string | null;
}): MentionPopupView {
  const key = mentionTokenKey(input.token);
  const limitHit = input.capped !== null && input.capped === key;
  const open =
    input.token !== undefined &&
    input.rows.length > 0 &&
    input.dismissed !== key;
  const activeIndex = Math.min(
    input.cursor,
    Math.max(input.rows.length - 1, 0),
  );
  return { open, activeIndex, activeId: input.rows[activeIndex]?.id, limitHit };
}

/** The picked list after a pick: appended in pick order, a repeat folded, and a
 *  pick past `MENTIONS_MAX` refused rather than silently dropped. `refused`
 *  says which happened so the picker can state the cap instead of looking
 *  broken; a repeat is not a refusal (the member is already placed). */
export interface MentionPick {
  readonly ids: readonly string[];
  readonly refused?: "limit";
}

export function addMention(picked: readonly string[], id: string): MentionPick {
  if (picked.includes(id)) return { ids: picked };
  if (picked.length >= MENTIONS_MAX) return { ids: picked, refused: "limit" };
  return { ids: [...picked, id] };
}

/** The picked list without one member. */
export function removeMention(
  picked: readonly string[],
  id: string,
): readonly string[] {
  return picked.filter((entry) => entry !== id);
}
