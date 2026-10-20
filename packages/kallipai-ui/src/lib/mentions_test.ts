// Table-driven tests for the mention picker's pure rules: where a token opens,
// which members a query offers, what a key does, and how a pick joins the list.
// The rules are the whole feature -- the component only holds state -- so every
// boundary that has been argued about is a row here.

import { assertNotEquals, assertEquals } from "@std/assert";
import type { MentionTarget } from "./room-message.ts";
import { MENTIONS_MAX } from "./room-message.ts";
import {
  addMention,
  mentionKeyIntent,
  MENTION_PICKER_LIMIT,
  mentionPopupView,
  type MentionPopupView,
  mentionTokenAt,
  mentionTokenKey,
  rankMentionCandidates,
  removeMention,
  stepMentionIndex,
} from "./mentions.ts";

const OPEN = { open: true, candidates: 3 };
const CLOSED = { open: false, candidates: 3 };
const EXHAUSTED = { open: true, candidates: 0 };

function key(
  over: Partial<{ key: string; isComposing: boolean; shiftKey: boolean }>,
) {
  return { key: "ArrowDown", isComposing: false, shiftKey: false, ...over };
}

function member(id: string, label?: string, handle?: string): MentionTarget {
  return { id, kind: "human", handle: handle ?? `@${label ?? id}`, label };
}

// --- the token boundary ----------------------------------------------------

const TOKEN_CASES: readonly {
  readonly name: string;
  readonly draft: string;
  readonly caret: number;
  readonly expected: { start: number; end: number; query: string } | undefined;
}[] = [
  {
    name: "start of the draft opens",
    draft: "@ad",
    caret: 3,
    expected: { start: 0, end: 3, query: "ad" },
  },
  {
    name: "after a space opens",
    draft: "hi @ad",
    caret: 6,
    expected: { start: 3, end: 6, query: "ad" },
  },
  {
    name: "after a line break opens",
    draft: "hi\n@ad",
    caret: 6,
    expected: { start: 3, end: 6, query: "ad" },
  },
  {
    name: "after Chinese text opens",
    draft: "你好@ali",
    caret: 6,
    expected: { start: 2, end: 6, query: "ali" },
  },
  {
    name: "after Chinese punctuation opens",
    draft: "你好，@ali",
    caret: 7,
    expected: { start: 3, end: 7, query: "ali" },
  },
  {
    name: "inside an address it stays text",
    draft: "ada@example",
    caret: 11,
    expected: undefined,
  },
  {
    name: "after an underscore it stays text",
    draft: "ada_@ali",
    caret: 8,
    expected: undefined,
  },
  {
    name: "a second at-sign does not open a token",
    draft: "hi @al@be",
    caret: 9,
    expected: undefined,
  },
  {
    name: "an empty query opens on the bare at-sign",
    draft: "hi @",
    caret: 4,
    expected: { start: 3, end: 4, query: "" },
  },
  {
    name: "a caret in the middle of the word still covers the word",
    draft: "hi @alice",
    caret: 5,
    expected: { start: 3, end: 9, query: "alice" },
  },
  {
    name: "a caret past the word has left the token",
    draft: "hi @al rest",
    caret: 10,
    expected: undefined,
  },
  {
    name: "a space ends the token",
    draft: "hi @al ",
    caret: 7,
    expected: undefined,
  },
  {
    name: "a caret at the very start is not inside a token",
    draft: "@ad",
    caret: 0,
    expected: undefined,
  },
  {
    name: "a caret before the first of two at-signs opens nothing",
    draft: "@al and @bo",
    caret: 0,
    expected: undefined,
  },
  {
    name: "a caret on the at-sign itself is not inside the token",
    draft: "hi @al",
    caret: 3,
    expected: undefined,
  },
  {
    name: "a trailing comma stays in the query, so nothing matches",
    draft: "hi @al,",
    caret: 7,
    expected: { start: 3, end: 7, query: "al," },
  },
];

for (const { name, draft, caret, expected } of TOKEN_CASES) {
  Deno.test(`token: ${name}`, () => {
    assertEquals(mentionTokenAt(draft, caret), expected);
  });
}

Deno.test("token: a caret outside the draft opens nothing", () => {
  assertEquals(mentionTokenAt("hi @al", -1), undefined);
  assertEquals(mentionTokenAt("hi @al", 12), undefined);
  assertEquals(mentionTokenAt("", 0), undefined);
});

// --- candidate ranking -----------------------------------------------------

const ROSTER: readonly MentionTarget[] = [
  member("id-1", "ada"),
  member("id-2", "adam"),
  member("id-3", "lada"),
  member("id-4", "bo"),
];

Deno.test("rank: prefix matches come before contains matches", () => {
  assertEquals(
    rankMentionCandidates(ROSTER, "ada").map((m) => m.label),
    ["ada", "adam", "lada"],
  );
});

Deno.test("rank: within a bucket the roster's order holds", () => {
  assertEquals(
    rankMentionCandidates(ROSTER, "a").map((m) => m.id),
    ["id-1", "id-2", "id-3"],
  );
});

Deno.test("rank: an empty query offers the roster, in order", () => {
  assertEquals(
    rankMentionCandidates(ROSTER, "").map((m) => m.id),
    ROSTER.map((m) => m.id),
  );
});

Deno.test(
  "rank: matching is case-insensitive over label, handle and id",
  () => {
    const roster = [member("ID-9", "Zoe", "@zoe")];
    assertEquals(rankMentionCandidates(roster, "zoe").length, 1);
    assertEquals(rankMentionCandidates(roster, "id-9").length, 1);
  },
);

Deno.test("rank: the list is capped", () => {
  const roster = Array.from({ length: 20 }, (_, i) => member(`id-${i}`));
  assertEquals(rankMentionCandidates(roster, "").length, MENTION_PICKER_LIMIT);
});

// --- the key state machine -------------------------------------------------

Deno.test("keys: a closed picker owns no key", () => {
  for (const k of ["ArrowDown", "ArrowUp", "Enter", "Tab", "Escape"]) {
    assertEquals(mentionKeyIntent(CLOSED, key({ key: k })), "none");
  }
});

Deno.test("keys: arrows navigate", () => {
  assertEquals(mentionKeyIntent(OPEN, key({ key: "ArrowDown" })), "next");
  assertEquals(mentionKeyIntent(OPEN, key({ key: "ArrowUp" })), "prev");
});

const STEP_CASES: readonly {
  readonly current: number;
  readonly length: number;
  readonly delta: number;
  readonly want: number;
}[] = [
  { current: 0, length: 3, delta: 1, want: 1 },
  { current: 2, length: 3, delta: 1, want: 0 },
  { current: 0, length: 3, delta: -1, want: 2 },
  { current: 1, length: 3, delta: -1, want: 0 },
  { current: 4, length: 3, delta: 2, want: 0 },
  { current: 0, length: 0, delta: 1, want: 0 },
];

Deno.test("step: the candidate ring wraps at both ends", () => {
  for (const c of STEP_CASES) {
    assertEquals(
      stepMentionIndex(c.current, c.length, c.delta),
      c.want,
      `from ${c.current} of ${c.length}, step ${c.delta}`,
    );
  }
});

Deno.test("keys: Enter and Tab accept a candidate", () => {
  assertEquals(mentionKeyIntent(OPEN, key({ key: "Enter" })), "accept");
  assertEquals(mentionKeyIntent(OPEN, key({ key: "Tab" })), "accept");
});

Deno.test("keys: Shift+Enter stays a line break", () => {
  assertEquals(
    mentionKeyIntent(OPEN, key({ key: "Enter", shiftKey: true })),
    "none",
  );
});

Deno.test("keys: with no candidate Enter sends the message", () => {
  assertEquals(mentionKeyIntent(EXHAUSTED, key({ key: "Enter" })), "none");
  assertEquals(mentionKeyIntent(EXHAUSTED, key({ key: "Tab" })), "none");
});

Deno.test("keys: Escape dismisses", () => {
  assertEquals(mentionKeyIntent(OPEN, key({ key: "Escape" })), "dismiss");
});

Deno.test("keys: a composing key belongs to the IME", () => {
  for (const k of ["ArrowDown", "ArrowUp", "Enter", "Tab", "Escape"]) {
    assertEquals(
      mentionKeyIntent(OPEN, key({ key: k, isComposing: true })),
      "none",
    );
  }
});

// --- the picked list -------------------------------------------------------

Deno.test("picks: a pick appends in order", () => {
  assertEquals(addMention([], "id-1").ids, ["id-1"]);
  assertEquals(addMention(["id-1"], "id-2").ids, ["id-1", "id-2"]);
});

Deno.test("picks: a repeat is folded, not refused", () => {
  const result = addMention(["id-1", "id-2"], "id-1");
  assertEquals(result.ids, ["id-1", "id-2"]);
  assertEquals(result.refused, undefined);
});

Deno.test("picks: the cap refuses and says so", () => {
  const full = Array.from({ length: MENTIONS_MAX }, (_, i) => `id-${i}`);
  assertEquals(addMention(full, "one-more").refused, "limit");
  assertEquals(addMention(full, "one-more").ids, full);
  // A repeat of an already-placed member still folds: the cap counts members.
  assertEquals(addMention(full, "id-3").refused, undefined);
  // One below the cap still accepts.
  assertEquals(addMention(full.slice(1), "one-more").refused, undefined);
});

Deno.test("picks: removing one keeps the order of the rest", () => {
  assertEquals(removeMention(["a", "b", "c"], "b"), ["a", "c"]);
  assertEquals(removeMention(["a"], "missing"), ["a"]);
});

// --- the popup's view ------------------------------------------------------

/** A token literal, so a case reads as the caret's position rather than as a
 *  constructor call. */
function token(start: number, end: number, query: string) {
  return { start, end, query };
}

Deno.test("keys: one token has one key, and no token has none", () => {
  assertEquals(mentionTokenKey(undefined), null);
  assertEquals(mentionTokenKey(token(3, 6, "al")), "3:6:al");
  assertEquals(
    mentionTokenKey(token(3, 6, "al")),
    mentionTokenKey(token(3, 6, "al")),
  );
  assertNotEquals(
    mentionTokenKey(token(3, 6, "ali")),
    mentionTokenKey(token(3, 6, "al")),
    "typing on is a different token",
  );
  assertNotEquals(
    mentionTokenKey(token(4, 6, "al")),
    mentionTokenKey(token(3, 6, "al")),
    "a token further along the draft is a different token",
  );
});

const POPUP_ROWS: readonly MentionTarget[] = [
  member("id-1", "ada"),
  member("id-2", "adam"),
];

const POPUP_CASES: readonly {
  readonly name: string;
  readonly token: ReturnType<typeof token> | undefined;
  readonly rows: readonly MentionTarget[];
  readonly cursor: number;
  readonly dismissed: string | null;
  readonly capped: string | null;
  readonly want: MentionPopupView;
}[] = [
  {
    name: "no token shows nothing",
    token: undefined,
    rows: [],
    cursor: 0,
    dismissed: null,
    capped: null,
    want: { open: false, activeIndex: 0, activeId: undefined, limitHit: false },
  },
  {
    name: "a token with candidates opens on the first row",
    token: token(3, 6, "ad"),
    rows: POPUP_ROWS,
    cursor: 0,
    dismissed: null,
    capped: null,
    want: { open: true, activeIndex: 0, activeId: "id-1", limitHit: false },
  },
  {
    name: "a token nothing matches stays shut",
    token: token(3, 6, "zz"),
    rows: [],
    cursor: 0,
    dismissed: null,
    capped: null,
    want: { open: false, activeIndex: 0, activeId: undefined, limitHit: false },
  },
  {
    name: "a cursor past a shortened list is clamped onto its last row",
    token: token(3, 6, "ad"),
    rows: POPUP_ROWS,
    cursor: 5,
    dismissed: null,
    capped: null,
    want: { open: true, activeIndex: 1, activeId: "id-2", limitHit: false },
  },
  {
    name: "an empty list keeps the cursor at the top",
    token: token(3, 6, "zz"),
    rows: [],
    cursor: 5,
    dismissed: null,
    capped: null,
    want: { open: false, activeIndex: 0, activeId: undefined, limitHit: false },
  },
  {
    name: "this token's dismissal keeps the popup shut",
    token: token(3, 6, "ad"),
    rows: POPUP_ROWS,
    cursor: 0,
    dismissed: "3:6:ad",
    capped: null,
    want: { open: false, activeIndex: 0, activeId: "id-1", limitHit: false },
  },
  {
    name: "another token's dismissal does not follow the caret here",
    token: token(3, 6, "ad"),
    rows: POPUP_ROWS,
    cursor: 0,
    dismissed: "3:6:adx",
    capped: null,
    want: { open: true, activeIndex: 0, activeId: "id-1", limitHit: false },
  },
  {
    name: "a refusal on this token shows the cap",
    token: token(3, 6, "ad"),
    rows: POPUP_ROWS,
    cursor: 0,
    dismissed: null,
    capped: "3:6:ad",
    want: { open: true, activeIndex: 0, activeId: "id-1", limitHit: true },
  },
  {
    name: "a refusal on another token is spent",
    token: token(3, 6, "ad"),
    rows: POPUP_ROWS,
    cursor: 0,
    dismissed: null,
    capped: "3:6:adx",
    want: { open: true, activeIndex: 0, activeId: "id-1", limitHit: false },
  },
];

for (const c of POPUP_CASES) {
  Deno.test(`popup: ${c.name}`, () => {
    assertEquals(
      mentionPopupView({
        token: c.token,
        rows: c.rows,
        cursor: c.cursor,
        dismissed: c.dismissed,
        capped: c.capped,
      }),
      c.want,
    );
  });
}
