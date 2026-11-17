// Thread grouping for the room transcript. A room line belongs to at most one
// discussion: the thread it declares, or the discussion of the row it quotes,
// or its own seq when it names neither (a head names the discussion it heads).
// Everything else about a discussion (its replies, whether they are all here)
// is what this module answers, so the view stays a renderer.

//
// Two questions live here and they are not the same: what the room stored
// (declaredThread) is what a read filters by, so it decides whether another
// page is worth asking for; where a reader sees a row belong
// (threadDiscussion) is resolved against the window, quotes included. A row
// whose resolution is not its declaration stored no root, so no read returns
// it: the panel draws that one from the window it already holds.

import { type RoomLine } from "./room-message.ts";

/** Where a discussion's root sits relative to the loaded window. A missing
 *  root has two opposite causes, and the view must not tell them apart wrong:
 *  `older` is a topic that started before the oldest loaded row (the orphan
 *  note is true), and `pending` is a topic the forward walk has not reached
 *  yet (its start is still on its way). `unknown` is neither: no row can
 *  place it, so the view says nothing rather than guessing. */
export type ThreadRootState = "rooted" | "older" | "pending" | "unknown";

/** One discussion as the transcript currently holds it. */
export interface ThreadGroup {
  /** The discussion's root seq, whether or not its root row is loaded. */
  readonly root: number;
  /** The row that gives the discussion its place in the transcript; drawn
   *  only when `rootState` is `rooted`. */
  readonly head: RoomLine;
  /** The loaded replies, in transcript order. */
  readonly replies: readonly RoomLine[];
  /** Whether the root row is loaded, and if not, which side of the window
   *  it fell off. See `ThreadRootState`. */
  readonly rootState: ThreadRootState;
  /** The replies the room holds: confirmed rows only, so `length` is a
   *  count the view may show. A pending or failed own line is in `replies`
   *  (the window holds it, so the panel can show it) but not in the room's
   *  log: counting it names a message nobody else received. */
  readonly confirmed: readonly RoomLine[];
  /** TRUE when `confirmed` is the whole set of replies, so its length may
   *  be shown as a count. Two facts make that true for a loaded root once
   *  the store has caught up with the room's present: a reply's seq is
   *  always greater than its root's (the root is written first), and a
   *  window that reaches the present holds every row after the head. */
  readonly repliesExact: boolean;
}

/** The discussion the room filed this line under: its declared thread, or its
 *  own seq when it declares none. A read filters by this root, so a page asked
 *  for it brings exactly the rows it names; the discussion a reader sees the
 *  line in may be wider (see `threadDiscussion`). */
export function declaredThread(line: RoomLine): number {
  return line.thread ?? line.seq;
}
/** The discussion a line belongs to, resolved against the rows on hand. A
 *  declared thread is the server's own answer and stands as written. A line
 *  that only quotes another is a reply to it: the room composer sends a quote
 *  with the discussion it joins, and a row stored before that change carries
 *  the quote alone, so the window walks from the quoted row to the discussion
 *  it belongs to. A quoted row the window does not hold leaves the quoted seq
 *  as the head the window can name, and a cycle of quotes stops at the row
 *  that closes it instead of looping. */
export function threadDiscussion(
  line: RoomLine,
  bySeq: ReadonlyMap<number, RoomLine>,
): number {
  const seen = new Set<number>();
  let current = line;
  for (;;) {
    if (current.thread !== undefined) return current.thread;
    const quoted = current.quote;
    if (quoted === undefined) return current.seq;
    if (seen.has(quoted)) return quoted;
    seen.add(quoted);
    const target = bySeq.get(quoted);
    if (target === undefined) return quoted;
    current = target;
  }
}

/** Rows by seq, the lookup a quote resolves through: a pending line has no
 *  position in the room's log, so nothing can quote it. */
export function threadIndex(
  ...windows: readonly (readonly RoomLine[])[]
): Map<number, RoomLine> {
  const index = new Map<number, RoomLine>();
  for (const lines of windows) {
    for (const line of lines) {
      if (line.seq >= 0) index.set(line.seq, line);
    }
  }
  return index;
}

/** The state the panel header may claim for the discussion it draws open. A
 *  root the panel actually draws is rooted, whatever the loaded window says:
 *  the discussion read reaches roots the window never held. Otherwise the
 *  grouping's own verdict stands, and a discussion no group places stays
 *  `unknown`. The test runs on the drawn rows, not on where they came from,
 *  so a later read path cannot make the header claim a root it does not draw. */
export function drawnRootState(
  root: number,
  drawn: readonly { readonly seq: number }[],
  grouped: ThreadRootState | undefined,
): ThreadRootState {
  if (drawn.some((line) => line.seq === root)) return "rooted";
  return grouped ?? "unknown";
}

/** Group the transcript into discussions, ordered by where each discussion's
 *  drawn head sits in the transcript. Input order is preserved inside a group,
 *  so a reply that arrives before its root still lands after it. An index the
 *  caller already built over the same window may be passed in; it must place
 *  every row of `lines`. */
export function threadGroups(
  lines: readonly RoomLine[],
  bySeq: ReadonlyMap<number, RoomLine> = threadIndex(lines),
): ThreadGroup[] {
  const position = new Map<RoomLine, number>();
  const buckets = new Map<number, RoomLine[]>();
  // The window's own bounds: the smallest and largest room-numbered row it
  // holds. A root outside them was never drawn here, and the side it fell
  // off says whether it is older than the window or still ahead of the walk.
  let lower: number | undefined;
  let upper: number | undefined;
  lines.forEach((line, index) => {
    position.set(line, index);
    if (line.seq > 0) {
      lower = lower === undefined ? line.seq : Math.min(lower, line.seq);
      upper = upper === undefined ? line.seq : Math.max(upper, line.seq);
    }
    const root = threadDiscussion(line, bySeq);
    const bucket = buckets.get(root);
    if (bucket === undefined) buckets.set(root, [line]);
    else bucket.push(line);
  });
  const groups: { group: ThreadGroup; at: number }[] = [];
  for (const [root, members] of buckets) {
    const rootAt = members.findIndex((line) => line.seq === root);
    let rootState: ThreadRootState = "unknown";
    if (rootAt >= 0) rootState = "rooted";
    else if (lower !== undefined && root < lower) rootState = "older";
    else if (upper !== undefined && root > upper) rootState = "pending";
    const head = rootAt >= 0 ? members[rootAt] : members[0];
    // A bucket always holds the line that created it, so `head` is defined;
    // the check names that to the type rather than asserting it away.
    if (head === undefined) continue;
    const replies = members.filter((line) => line !== head);
    const confirmed = replies.filter((line) => line.seq >= 0 && !line.failed);
    groups.push({
      group: {
        root,
        head,
        replies,
        confirmed,
        rootState,
        repliesExact: rootState === "rooted",
      },
      at: position.get(head) ?? 0,
    });
  }
  return groups.sort((a, b) => a.at - b.at).map((entry) => entry.group);
}

/** The discussions whose head this window holds, in transcript order: the only
 *  ones a standing has a reader for. A group whose head fell off the window
 *  (`older`) or has not arrived (`pending`) shows no mute mark in the preview,
 *  so asking for its standing spends one request per thread on an answer
 *  nothing shows. */
export function rootedRoots(groups: readonly ThreadGroup[]): number[] {
  return groups
    .filter((group) => group.rootState === "rooted")
    .map((group) => group.root);
}

/** The words a line carries from its sender: what a pending line and the row
 *  the room numbered for it have in common. */
function words(line: RoomLine): string {
  return JSON.stringify([line.senderId, line.text]);
}

/** The rows the discussion panel draws: the pages it fetched, plus the rows the
 *  room already placed inside the stretch those pages cover, which a read may
 *  not return. An own pending send has no seq yet, and a reply whose declared
 *  head did not exist when it was written stores no root, so the read's
 *  stored-root filter never sees it while the room's entry counts it. A row
 *  that only quotes another is one too (see `threadDiscussion`). Rows are
 *  deduplicated by seq and by sender+text -- the last pending line with those
 *  words and its confirmed echo are one message; the pending ones are drawn
 *  last, below an own reply the room confirmed past the stretch.
 *
 *  `coveredTo` is the end of the stretch the panel has read forward to: the
 *  cursor of its last page, carried forward over the room rows it already
 *  holds, so a reply that lands after that read still joins when nothing
 *  between is missing (see `threadWindowEnd`). A room row beyond it is not
 *  drawn: it would sit across a stretch nobody fetched, and the walk only
 *  goes down. A pending line has no position, so it joins at the end
 *  whatever the stretch is. A row the pages cannot bring joins there too,
 *  above the pending ones: an own reply the room confirmed, or one whose
 *  discussion only a quote decides. Each keeps a place at the foot of the
 *  window rather than being drawn nowhere. */
export function threadRows(
  root: number,
  loaded: readonly RoomLine[],
  fetched: readonly RoomLine[],
  coveredTo: number | null,
): RoomLine[] {
  const bySeq = threadIndex(loaded, fetched);

  // A pending line and its echo carry the same words from the same sender.
  // The last pending with those words is the one an echo claims, and only a
  // row a page brings as a new arrival counts as an echo: an older own reply
  // repeating the words is a row the window already holds, keeps its own
  // place and leaves the new send drawn. No clock is compared -- the pending's
  // time comes from this client, the echo's from the room.
  const lastPending = new Map<string, RoomLine>();
  for (const line of loaded) {
    if (line.seq < 0 && threadDiscussion(line, bySeq) === root) {
      lastPending.set(words(line), line);
    }
  }
  const arrivals = fetched.filter(
    (row) => !loaded.some((held) => held.seq === row.seq),
  );
  const placed = loaded.filter((line) => {
    if (threadDiscussion(line, bySeq) !== root) return false;
    if (fetched.some((row) => row.seq === line.seq)) return false;
    if (line.seq < 0) {
      if (lastPending.get(words(line)) !== line) return true;
      return !arrivals.some((row) => words(row) === words(line));
    }
    return coveredTo !== null && line.seq >= root && line.seq <= coveredTo;
  });
  // Two kinds of row step ahead of the pages, and the rule is narrow on
  // purpose. An own reply the room confirmed keeps the place it was sent from,
  // so its echo does not leave the window the reader just saw it in. A row
  // whose discussion only a quote decides stored no root, so a read cannot
  // return it: the panel is the only place it can be drawn. Everyone else
  // waits for a page, because a page returns every row that declared this
  // discussion and still sat where it said. Drawing those here would put a
  // plain reply at the foot and move it back into order later, and would let a
  // row jump a hole in the window.
  const beyond = loaded
    .filter((line) => {
      if (line.seq <= 0) return false;
      // The discussion is resolved, not read off the declaration: a row that
      // only quotes into this one belongs here too.
      const resolves = threadDiscussion(line, bySeq);
      if (resolves !== root) return false;
      if (line.mine !== true && resolves === declaredThread(line)) return false;
      // A null edge sets no limit: these two kinds stay drawn even before
      // the first page lands.
      if (coveredTo !== null && line.seq <= coveredTo) return false;
      return true;
    })
    .sort((a, b) => a.seq - b.seq);
  const numbered = placed.filter((line) => line.seq >= 0);
  const pending = placed.filter((line) => line.seq < 0);
  const ordered = [...fetched, ...numbered].sort((a, b) => a.seq - b.seq);
  return [...ordered, ...beyond, ...pending];
}

/** One page of a discussion as the store hands it back. `lastSeq` is the
 *  page's own cursor, the highest seq it returned whether or not a row
 *  survived decoding; `hasMore` says whether another page exists. The page
 *  gives both, so a caller never infers a cursor from the rows it kept. */
export interface ThreadPage {
  readonly lines: RoomLine[];
  readonly lastSeq: number | null;
  readonly hasMore: boolean;
}

/** What the foot of a discussion panel has to say, as one value rather
 *  than a set of flags whose exclusivity something else has to keep: a page
 *  being taken, a page that failed, a walk stopped at the page cap, a row no
 *  page ever drew, rows the room still holds that no page will bring, or
 *  nothing. */
export type ThreadMore =
  | "idle"
  | "loading"
  | "failed"
  | "stopped"
  | "missing"
  | "unreachable";

/** Keep a page below the window already read. A row an earlier page
 *  already drew is kept once, by seq; the cursor moves with the page and
 *  never backwards, and a page that names no cursor leaves the old one. */
export function appendThreadPage(
  read: ThreadPage | null,
  page: ThreadPage,
): ThreadPage {
  if (read === null) return page;
  const seen = new Set(read.lines.map((line) => line.seq));
  return {
    lines: [...read.lines, ...page.lines.filter((line) => !seen.has(line.seq))],
    lastSeq: page.lastSeq ?? read.lastSeq,
    hasMore: page.hasMore,
  };
}

/** The end of the stretch the panel may draw: its last cursor, carried
 *  forward over the room rows it already holds. A reply lands after a page
 *  was read, the reader's own send or somebody else's, and the panel still
 *  shows it as long as every seq between is in the room window: no stretch
 *  was skipped, so no hole opens. A room window that has not caught up
 *  stops the carry at its first gap, and the row joins when they arrive. */
export function threadWindowEnd(
  lastSeq: number | null,
  loaded: readonly RoomLine[],
): number | null {
  if (lastSeq === null) return null;
  const known = new Set(
    loaded.filter((line) => line.seq >= 0).map((line) => line.seq),
  );
  let end = lastSeq;
  while (known.has(end + 1)) end += 1;
  return end;
}

/** How many rows of this discussion the room's window holds past the stretch
 *  the panel's pages drew, ending at `coveredTo` (what `threadWindowEnd`
 *  reports for the read's cursor): the last page may have been the end of
 *  what was read while the room moved on and took more replies. Such a row
 *  is a page still worth asking for, whatever the last page's cursor said. */
export function threadRoomBeyondCount(
  root: number,
  coveredTo: number | null,
  loaded: readonly RoomLine[],
): number {
  if (coveredTo === null) return 0;
  return loaded.filter(
    // Declared roots, as above: this count is the reason to ask the room for one
    // more page, and the read filters by the root it stored. A row that only
    // resolves into this discussion is one no page for it can bring, so counting
    // it would ask for a page that answers with what the reader already has.
    (line) => line.seq > coveredTo && declaredThread(line) === root,
  ).length;
}

/** What a landing page claims of the rows the room showed past the stretch
 *  when it was asked for, counted then (`beyondBefore`). A short page under
 *  such a row is the server's last word for its cursor: no later page starts
 *  below that row, so it is a row the read never draws, and asking again at
 *  the same cursor answers with the same page. The count is what the verdict
 *  is taken back on: rows the room takes since are a fresh claim and earn one
 *  more page, so a verdict never outlives the evidence it answered for. A
 *  page that says more claims nothing, and neither does a request that saw
 *  nothing past the stretch. Counting as the page lands instead would put a
 *  reply that arrived in flight into the claim, as a row the page answered
 *  for: the claim would match the room at once and that reply would wait for
 *  the discussion to be opened again. */
export function threadPageClaim(
  read: { readonly hasMore: boolean },
  beyondBefore: number,
): number | null {
  return read.hasMore || beyondBefore === 0 ? null : beyondBefore;
}

/** What the panel's paging gate and its foot make of one read against the
 *  room, from `beyond` (the rows of this discussion the room places past the
 *  stretch the pages drew) and `claim` (`threadPageClaim` of the last page to
 *  land). `spent` stands when a short page already answered for exactly those
 *  rows, so no page is worth asking for and the foot has something to explain;
 *  `more` is whether another page is worth asking for at all. A row of this
 *  discussion the room takes lands past the claim: `beyond` moves, the match
 *  breaks, and the gate opens for the page that brings that row in, so a foot
 *  already naming the rows it cannot draw takes the sentence back on its own.
 *  That is cheaper than a latch, which would keep such a row out of the panel
 *  until the discussion was opened again. */
export function threadPaging(
  read: { readonly hasMore: boolean } | null,
  beyond: number,
  claim: number | null,
): { readonly spent: boolean; readonly more: boolean } {
  const spent = claim !== null && claim === beyond && beyond > 0;
  return { spent, more: read?.hasMore === true || (beyond > 0 && !spent) };
}

/** Whether the panel has read the discussion to its end: the last page it took
 *  was short. A window still holding a full page, or no window at all, is not
 *  whole, and a caller may not state a count or a membership reason from it. */
export function threadAtEnd(
  read: { readonly hasMore: boolean } | null,
): boolean {
  return read?.hasMore === false;
}

/** What the transcript entry for a discussion may say: a number and the last
 *  reply only when the count is known to be the room's whole count. The count
 *  comes from confirmed rows alone (a pending or failed own line is not in the
 *  room), and a window that has caught up is what makes the loaded head's
 *  replies the whole set -- a number claims the whole set, so it waits for
 *  `caughtUp`. The preview under it does not (see `threadPreview`). Otherwise
 *  the entry is the plain invitation to open the discussion, and names nobody:
 *  a wrong last sender is worse than none. */
export interface ThreadEntry {
  readonly count?: number;
  readonly last?: RoomLine;
}

export function threadEntry(
  group: ThreadGroup,
  caughtUp: boolean,
): ThreadEntry {
  if (!caughtUp || !group.repliesExact || group.confirmed.length === 0) {
    return {};
  }
  return { count: group.confirmed.length, last: group.confirmed.at(-1) };
}

/** The start of a discussion, as the room list draws it inside the bubble: the
 *  first replies in transcript order, so that opening the discussion carries on
 *  from where the preview stopped. Its rows come from `confirmed` alone -- a
 *  pending or failed line of the reader's own is not in the room, and a preview
 *  is the one place a message nobody received could pass for a message that was.
 *  A row with nothing to say (an empty body) is skipped rather than drawn as a
 *  gap.
 *
 *  One gate, `group.repliesExact`: the head of this discussion is among the lines
 *  this client holds (`repliesExact` is `rootState === "rooted"`). Its three
 *  ways of being false -- the head lies below the loaded window, above it, or the
 *  window's own bounds are unknown -- all leave replies with no start in hand,
 *  and showing those first would call a stretch of the middle the beginning, the
 *  one claim the input cannot back. `caughtUp` is not part of this gate: the
 *  count asks whether it has them all, the preview asks only whether these are
 *  the first. They are, because `confirmed` holds only rows the forward walk
 *  delivered and that walk starts at the room's first row -- so its rows are the
 *  discussion's earliest replies, whatever the window's top edge. The window's
 *  tail may hold a live row the walk never reached (a synthetic sequence number,
 *  which `confirmed` drops), which is why this rests on `confirmed` and not on
 *  the window. A big room therefore shows the replies before it can state a
 *  count, which is the shape a reader needs to judge whether the discussion is
 *  worth opening.
 *
 *  A row draws the message's source rather than the rendered markdown an agent's
 *  reply gets on the discussion page: the clamp needs a line count it can trust,
 *  and a rendered block would re-flow its own emphasis and fences into the
 *  bubble. The cost is that a reader sees `**` where the discussion shows bold;
 *  that is the price of the clamp, not a bug to fix.
 *  Pure: unit-tested in threads_test.ts. */
export interface ThreadPreview {
  readonly rows: readonly RoomLine[];
}

/** How many replies a preview draws before the discussion page takes over. */
const PREVIEW_ROWS = 3;

export function threadPreview(group: ThreadGroup): ThreadPreview {
  if (!group.repliesExact) return { rows: [] };
  const sayable = group.confirmed.filter((row) => row.text.trim() !== "");
  return { rows: sayable.slice(0, PREVIEW_ROWS) };
}

/** How the room list draws one group. A discussion the reader can see a body
 *  for is drawn as a bubble that IS its entry: the whole row opens it, and the
 *  reply line under the copy controls moves into the panel with them. A group
 *  whose head is out of the window draws no body at all, so it keeps the plain
 *  entry button and its note -- there is nothing there to click. Pure:
 *  unit-tested in threads_test.ts. */
export function roomRowShape(group: ThreadGroup): "plain" | "thread" | "entry" {
  if (group.rootState !== "rooted") return "entry";
  return group.replies.length > 0 ? "thread" : "plain";
}

/** Why the reader is in a discussion, as the panel header says it. */
export type ThreadMembership = "started" | "replied" | "mentioned";

/** Why the reader is in this discussion: they started it, they replied in
 *  it, or someone named them in it. Starting beats replying beats being
 *  named -- each is the stronger claim. Null when none of the three holds,
 *  and null when the read could not see the whole discussion: a half
 *  discussion can hide the row that names them, and the panel would then
 *  explain the wrong reason for being there. */
export function threadMembership(
  root: number,
  lines: readonly RoomLine[],
  selfId: string | null,
  complete: boolean,
): ThreadMembership | null {
  if (!complete || selfId === null) return null;
  let replied = false;
  let mentioned = false;
  for (const line of lines) {
    if (line.seq === root && line.senderId === selfId) return "started";
    if (line.senderId === selfId) replied = true;
    if (line.mentions?.includes(selfId) === true) mentioned = true;
  }
  if (replied) return "replied";
  if (mentioned) return "mentioned";
  return null;
}
