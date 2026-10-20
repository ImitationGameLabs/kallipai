// The mention picker's live state: which members are picked, which candidate is
// active, and whether the popup is open. Every rule it applies lives in
// `mentions.ts` (pure and table-tested), down to what the popup shows: this
// module holds the state one keyboard session needs and calls those rules,
// wiring them to the draft the composer owns, so a page gains the whole feature
// by passing five closures (scope, members, draft, caret, dropToken).
//
// The popup is derived, not toggled: it is open exactly while the caret sits in
// an `@` token, the token offers a candidate, and the user has not dismissed
// this very token. Typing on after Escape changes the token and reopens it,
// which is what dismissing one completion should mean.
//
// State is read through those rules rather than cached in derived state: the
// readers are a render and a key handler, the inputs are five closures and five
// small cells, and a live computation is the one a test can run (a `$derived`
// would freeze at construction once the runes are shimmed). `scope` and
// `members` arrive as getters for the same reason: every read goes through
// them, so a snapshot taken at construction would freeze the picker.
//
// Two rules settle where every piece of state lives. What the composer owns --
// the draft, the picked members, the armed quote -- belongs to its
// conversation: a swap keeps it, and the reader shows the live room's own
// entry (the draft outlives the page in sessionStorage; the picks and the quote
// live as long as the mounted page). What a token session owns -- the cursor,
// the dismissal, the cap notice -- is about this draft's token: the reads
// compare the scope, so a swap hides the popup, and the next write drops the
// snapshots.

import {
  addMention,
  type MentionKey,
  mentionKeyIntent,
  mentionPopupView,
  mentionTokenAt,
  mentionTokenKey,
  rankMentionCandidates,
  removeMention,
  stepMentionIndex,
} from "./mentions.ts";
import type { MentionTarget } from "./room-message.ts";

/** The popup's view over state that belongs to one conversation. */
interface PickerView {
  readonly open: boolean;
  readonly rows: readonly MentionTarget[];
  readonly activeIndex: number;
  readonly activeId: string | undefined;
  readonly limitHit: boolean;
}

/** One conversation's picks: the member ids, and the scope holding them. */
interface ScopePicks {
  readonly scope: string | undefined;
  readonly ids: readonly string[];
}

const NO_ROWS: readonly MentionTarget[] = [];
const NO_PICKS: readonly string[] = [];
/** A scope no conversation can equal, so the first read or write owns. */
const UNOWNED = Symbol("unowned");

export interface MentionPickerOptions {
  /** The conversation these picks belong to: the reader shows this scope's
   *  own picks, and a different value drops this session's token snapshots. A
   *  getter, so every read goes through the live route.
   *  Compared by value: one conversation reports the same value for as long as
   *  the page lives (the hosts pass their route id). */
  readonly scope: () => string | undefined;
  /** The room's roster, as the page already holds it (no fetch of its own). */
  readonly members: () => readonly MentionTarget[];
  /** The composer draft, read reactively. */
  readonly draft: () => string;
  /** The composer's caret, read reactively. */
  readonly caret: () => number;
  /** Cut the token out of the draft and leave the caret where it started. */
  readonly dropToken: (start: number, end: number) => void;
}

export interface MentionPicker {
  /** Whether the candidate popup is showing. */
  readonly open: boolean;
  /** The candidates on offer, capped and ordered. */
  readonly rows: readonly MentionTarget[];
  readonly activeIndex: number;
  /** The active candidate's id, for `aria-activedescendant`. */
  readonly activeId: string | undefined;
  /** The members the user picked, in pick order. */
  readonly picked: readonly string[];
  /** Whether the last pick was refused by the per-message cap. */
  readonly limitHit: boolean;
  /** Handle one key press; true means the picker consumed it. */
  key: (event: MentionKey) => boolean;
  pick: (id: string) => void;
  remove: (id: string) => void;
  /** Drop the last pick -- the backspace-on-empty-draft gesture. */
  removeLast: () => void;
  /** Forget every pick (a send spends them). */
  clear: () => void;
}

export function createMentionPicker(
  options: MentionPickerOptions,
): MentionPicker {
  /** The picks held per scope. The composer's own state belongs to its
   *  conversation, so a swap keeps them; only a send or a removal spends one
   *  scope's entry. */
  let picks = $state<readonly ScopePicks[]>([]);
  let cursor = $state(0);
  // The token a dismissal applies to, and the token a cap refusal applies to:
  // both are snapshots, so moving to another token clears the effect by itself
  // instead of needing a reset the caller could forget.
  let dismissed = $state<string | null>(null);
  let capped = $state<string | null>(null);
  let scope: string | undefined | typeof UNOWNED = UNOWNED;

  /** Take ownership of the live scope, dropping this session's snapshots: the
   *  cursor, the dismissal and the cap notice. The picks stay, because they are
   *  the draft's and not the session's. Every write goes through here; the
   *  reads compare instead, because a render may not write. */
  function own(): void {
    const next = options.scope();
    if (next === scope) return;
    scope = next;
    cursor = 0;
    dismissed = null;
    capped = null;
  }

  /** One scope's picks, empty where the user made none there. */
  function picksIn(target: string | undefined): readonly string[] {
    const held = picks.find((entry) => entry.scope === target);
    return held === undefined ? NO_PICKS : held.ids;
  }

  /** Replace one scope's picks, leaving every other scope's as they were. A
   *  scope whose list empties leaves no entry behind. The store is an immutable
   *  array because `$state` does not track a plain `Map`'s mutations. */
  function holdPicks(target: string | undefined, ids: readonly string[]): void {
    const held = picks.filter((entry) => entry.scope !== target);
    picks = ids.length === 0 ? held : held.concat({ scope: target, ids });
  }

  /** The popup's view for the live scope. A scope this controller has not
   *  owned yet -- the page just swapped rooms -- is read as a session with no
   *  history: the token is still the user's, so the popup opens, but nothing
   *  the previous conversation left behind (cursor, dismissal, cap) is read. */
  function view(): PickerView {
    const owned = scope === options.scope();
    const token = mentionTokenAt(options.draft(), options.caret());
    const rows =
      token === undefined
        ? NO_ROWS
        : rankMentionCandidates(options.members(), token.query);
    const popup = mentionPopupView({
      token,
      rows,
      cursor: owned ? cursor : 0,
      dismissed: owned ? dismissed : null,
      capped: owned ? capped : null,
    });
    return {
      open: popup.open,
      rows,
      activeIndex: popup.activeIndex,
      activeId: popup.activeId,
      limitHit: popup.limitHit,
    };
  }

  function take(id: string): void {
    const token = mentionTokenAt(options.draft(), options.caret());
    const target = options.scope();
    const result = addMention(picksIn(target), id);
    if (result.refused === "limit") {
      // Nothing is dropped from the draft: the user still sees what they typed
      // and the popup says why it did not take.
      capped = mentionTokenKey(token);
      return;
    }
    holdPicks(target, result.ids);
    cursor = 0;
    capped = null;
    if (token !== undefined) options.dropToken(token.start, token.end);
  }

  return {
    get open() {
      return view().open;
    },
    get rows() {
      return view().rows;
    },
    get activeIndex() {
      return view().activeIndex;
    },
    get activeId() {
      return view().activeId;
    },
    get picked() {
      return picksIn(options.scope());
    },
    get limitHit() {
      return view().limitHit;
    },
    key(event: MentionKey) {
      own();
      const live = view();
      const intent = mentionKeyIntent(
        { open: live.open, candidates: live.rows.length },
        event,
      );
      if (intent === "none") return false;
      if (intent === "next") {
        cursor = stepMentionIndex(live.activeIndex, live.rows.length, 1);
        return true;
      }
      if (intent === "prev") {
        cursor = stepMentionIndex(live.activeIndex, live.rows.length, -1);
        return true;
      }
      if (intent === "dismiss") {
        dismissed = mentionTokenKey(
          mentionTokenAt(options.draft(), options.caret()),
        );
        cursor = 0;
        return true;
      }
      const row = live.rows[live.activeIndex];
      if (row !== undefined) take(row.id);
      return true;
    },
    pick(id: string) {
      own();
      take(id);
    },
    remove(id: string) {
      own();
      const target = options.scope();
      holdPicks(target, removeMention(picksIn(target), id));
      // A member came off the message, so the cap has room again and a notice
      // about the cap would be stale.
      capped = null;
    },
    removeLast() {
      own();
      const target = options.scope();
      holdPicks(target, picksIn(target).slice(0, -1));
      capped = null;
    },
    clear() {
      own();
      holdPicks(options.scope(), NO_PICKS);
      cursor = 0;
      capped = null;
      dismissed = null;
    },
  };
}
