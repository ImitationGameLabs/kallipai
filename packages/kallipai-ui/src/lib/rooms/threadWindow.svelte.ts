// ThreadWindow: one discussion's pages, and the reads that take them.
//
// Two surfaces read a discussion: the room page's panel, and the discussion's
// own page. Both need the same window -- the first page asked for by position,
// the next ones by cursor, the walk down to a row the window does not hold,
// the generation guards that keep a page from landing in another discussion,
// and the paging gate that knows when the room holds rows no page will bring.
// Two copies of that would drift, so it lives here; which discussion is open
// belongs to the surface (`root` is its getter).
//
// Lifecycle belongs to the surface too: nothing else in the package's rune
// modules owns an effect (`createAutoScroll`, `createReveal` both leave that
// to their component), so a surface that can change its root calls `sync()`
// from its own effect and takes the result as state.

import { tick } from "svelte";
import { rememberThreadHeadOf } from "./threadHeads.svelte.ts";
import type { RoomLine } from "../room-message.ts";
import { archeionSession } from "../session/archeion.svelte";
import {
  roomConversationsStore,
  THREAD_WALK_MAX_PAGES,
} from "../session/roomConversations.svelte.ts";
import {
  appendThreadPage,
  threadAtEnd,
  threadMembership,
  threadPageClaim,
  threadPaging,
  threadRoomBeyondCount,
  threadRows,
  threadWindowEnd,
  type ThreadMore,
  type ThreadPage,
} from "../threads.ts";

/** How a walk down a discussion ended: on the row it was sent for, at the
 *  discussion's end without drawing that row, at the page cap, on a page that
 *  failed, or on a window that is no longer the one that asked. Only `found`
 *  lands on a row; `failed` keeps its target, so the retry can carry the walk
 *  on. */
export type WalkOutcome = "found" | "end" | "capped" | "failed" | "gone";

export interface ThreadWindowOptions {
  /** The room the discussion lives in. Read on every command: the surface's
   *  room can change, and a page must never land in the room next door. */
  roomId: () => string;
  /** The discussion this surface has open, or null for none. */
  root: () => number | null;
  /** The room's loaded lines, which place the discussion's rows past the
   *  pages and tell the paging gate what no page will bring. */
  lines: () => readonly RoomLine[];
  /** The reader's participant id, for the membership line. Defaults to the
   *  archeion session's. */
  selfId?: () => string | null;
  /** Where a finished walk lands: the surface jumps its own reveal. */
  onLanded?: (seq: number) => void;
  /** How a page is read. Defaults to the conversation store's thread read;
   *  a test can pass its own. */
  fetchPage?: (
    room: string,
    root: number,
    afterSeq: number,
  ) => Promise<ThreadPage>;
}

/** How many 25ms waits a walk gives a page already in flight before it gives
 *  up: ten seconds, well past a page that is merely slow. */
const WALK_WAIT_LIMIT = 400;

/**
 * One discussion's window: the pages read so far, and the commands that take
 * more. Call it at component init (it builds `$derived` cells); a surface
 * whose root can change calls `sync()` from an effect, and `clear()` when it
 * closes the discussion itself.
 */
export function createThreadWindow(options: ThreadWindowOptions) {
  const { roomId, root: rootOf, lines: roomLinesOf, onLanded } = options;
  const selfId = options.selfId ?? (() => archeionSession.participantId);
  const fetchPage =
    options.fetchPage ??
    ((room: string, root: number, afterSeq: number) =>
      roomConversationsStore.fetchThreadPage(room, root, afterSeq));

  let read = $state<ThreadPage | null>(null);
  // How many pages have landed, the first one included: the surface tells its
  // own fetches from rows the room brings in, and only the latter are
  // followed.
  let pages = $state(0);
  let loading = $state(false);
  let unreadable = $state(false);
  let loadingMore = $state(false);
  let moreFailed = $state(false);
  // One walk down the discussion toward a row the window does not hold yet.
  // The target outlives a walk that stopped at the page cap, so the reader can
  // ask for the next stretch from where it stopped.
  let walkTarget = $state<number | "newest" | null>(null);
  // A walk is in flight: the surface follows it down, so the reader sees the
  // pages arrive rather than a count off screen.
  let walking = $state(false);
  let walkStopped = $state(false);
  let walkMissing = $state(false);
  // Rows of this discussion the room holds past the read that no page will
  // bring, counted as they stood when a short page answered for them. That
  // count is the key a verdict is taken back on: rows the room takes since are
  // a fresh claim and earn one more page, where a latch would keep them out of
  // the window until the discussion was opened again.
  let spentClaim = $state<number | null>(null);

  // The end of the stretch the window may draw: the pages' last cursor
  // carried forward over the room rows already held.
  const covered = $derived(
    threadWindowEnd(read?.lastSeq ?? null, roomLinesOf()),
  );

  // How many rows of this discussion the room places past that stretch: a page
  // still worth asking for, until a short page answers for them.
  const beyond = $derived(
    rootOf() === null
      ? 0
      : threadRoomBeyondCount(rootOf() as number, covered, roomLinesOf()),
  );

  // What the open read makes of the paging gate and the foot, from the rows the
  // room places past the stretch and the count a short page answered for.
  const pagingNow = $derived(threadPaging(read, beyond, spentClaim));
  const more = $derived.by((): ThreadMore => {
    if (walkMissing) return "missing";
    if (walkStopped) return "stopped";
    if (moreFailed) return "failed";
    if (loadingMore) return "loading";
    if (pagingNow.spent) return "unreachable";
    return "idle";
  });

  // The rows drawn: the pages read so far, plus the room's own rows of this
  // discussion inside the stretch those pages cover, which a read may not
  // return (an own pending send; a reply whose declared head was missing when
  // it was written, and so stored no root).
  const lines = $derived.by((): readonly RoomLine[] => {
    const root = rootOf();
    if (root === null) return [];
    return threadRows(root, roomLinesOf(), read?.lines ?? [], covered);
  });

  // The window asks for a page while the read says more, and while the room
  // holds a row of this discussion past the stretch its pages drew; it stops
  // once a short page has answered for exactly those rows.
  const hasMore = $derived(pagingNow.more);

  // Whether the window has read the discussion to its end. A window that stops
  // on a full page, or one the room still holds rows of, may not present
  // itself as the whole.
  const isComplete = $derived(threadAtEnd(read) && beyond === 0);

  // Why the reader is in the open discussion: null when the read could not see
  // the whole discussion, so a half discussion never explains the wrong
  // reason.
  const membership = $derived.by(() => {
    const root = rootOf();
    if (root === null) return null;
    return threadMembership(root, lines, selfId(), isComplete);
  });

  // The window's generation: one per reset. A page or a walk started before a
  // reset answers a window that is gone, and identity alone cannot tell: a
  // reader who leaves `/5` for `/6` and comes back to `/5` names the same pair,
  // so the first visit's page would be judged on screen and land in the second
  // visit's window with its old cursor (see `holds`).
  let generation = 0;

  /** Whether the window still holds what a read was sent for: the generation
   *  taken when it started, plus the pair read from the getters. Never a
   *  captured room or root alone -- a surface that changed its root mid-flight
   *  must not have the old discussion's page land, and a root it came back to
   *  is a new window, not the one the read left. */
  function holds(gen: number, room: string, root: number): boolean {
    return gen === generation && roomId() === room && rootOf() === root;
  }

  /** Everything the window holds of a discussion, back to nothing. Opening
   *  another discussion and closing this one differ only in what the surface
   *  leaves behind, so the state is reset in one place. */
  function reset(): void {
    generation += 1;
    read = null;
    pages = 0;
    loading = false;
    unreadable = false;
    loadingMore = false;
    moreFailed = false;
    spentClaim = null;
    walkTarget = null;
    walkStopped = false;
    walkMissing = false;
    walking = false;
  }

  /** Read one page and keep it only if this window asked for it: a page that
   *  lands after another discussion opened answers a surface that is gone, and
   *  drawing it would put one discussion's rows under another's head. The room
   *  is captured with the root: a page must never land in the room next
   *  door. */
  async function readThreadPage(
    gen: number,
    room: string,
    root: number,
    afterSeq: number,
  ): Promise<ThreadPage | null> {
    const page = await fetchPage(room, root, afterSeq);
    return holds(gen, room, root) ? page : null;
  }

  /** Record what a landing page claims, from the count taken before the ask:
   *  both fetches measure the rows past the stretch first and hand that count
   *  here, so a reply that arrives while the page is in flight is not swallowed
   *  as one the page answered for. See `threadPageClaim`. */
  function recordSpentClaim(
    page: { readonly hasMore: boolean },
    beyondBefore: number,
  ): void {
    spentClaim = threadPageClaim(page, beyondBefore);
  }

  /** Open one discussion: the window shows its loading state at once, then the
   *  page the root sits on lands. The root's own page is asked for by
   *  position, so a discussion opens in one round trip however long it is. */
  async function openThread(root: number): Promise<void> {
    const room = roomId();
    reset();
    const gen = generation;
    loading = true;
    try {
      const page = await readThreadPage(gen, room, root, root - 1);
      if (page !== null) {
        read = page;
        pages += 1;
        // A first page has no drawn stretch before it, so no row it could have
        // answered for: it claims nothing.
        recordSpentClaim(page, 0);
        // The trail names this discussion by its first line, and it is
        // drawn before this window has read anything: record it here,
        // where the root's page is the one in hand.
        const head = page.lines[0];
        if (head !== undefined && head.seq === root) {
          rememberThreadHeadOf(room, root, head);
        }
      }
    } catch (e) {
      console.error("[room thread] first page failed:", e);
      if (holds(gen, room, root)) unreadable = true;
    } finally {
      if (holds(gen, room, root)) loading = false;
    }
  }

  /** Read the open discussion for the first time, or again after a page that
   *  failed. */
  async function retryThread(): Promise<void> {
    const root = rootOf();
    if (root === null) return;
    await openThread(root);
  }

  /** Take the page below what the window holds. One page per call; the read's
   *  `hasMore` says whether another exists. `walkTo` and the surface's own
   *  scrolling both come through here, and both need to know whether a page
   *  was actually taken: a call that returns while another page is in flight
   *  fetched nothing, and a walk that counted it would spend its budget on
   *  pages it never asked for. */
  async function loadMoreThread(): Promise<"page" | "none" | "failed"> {
    const root = rootOf();
    const held = read;
    if (root === null || held === null) return "none";
    if (loadingMore) return "none";
    // The last page can be short and the discussion still be behind the room:
    // a reply taken since sits past the stretch those pages drew.
    if (!hasMore) return "none";
    const room = roomId();
    const gen = generation;
    const cursor = held.lastSeq ?? root - 1;
    // The rows of this discussion the room shows past the stretch before the
    // ask: a short page under them is the server's last word for this cursor,
    // and this count is what that verdict is taken back on. Measured here, so a
    // reply taken while the page is in flight is not claimed as answered.
    const beyondBefore = threadRoomBeyondCount(root, covered, roomLinesOf());
    loadingMore = true;
    moreFailed = false;
    try {
      const page = await readThreadPage(gen, room, root, cursor);
      if (page === null) return "none";
      const next = appendThreadPage(read, page);
      read = next;
      recordSpentClaim(next, beyondBefore);
      pages += 1;
      // A page landed, so whatever the foot said about where the window stopped
      // is out of date.
      walkStopped = false;
      return "page";
    } catch (e) {
      console.error("[room thread] next page failed:", e);
      if (holds(gen, room, root)) moreFailed = true;
      return "failed";
    } finally {
      if (holds(gen, room, root)) loadingMore = false;
    }
  }

  /** The retry after a page failed. A walk that ran into the failure still
   *  holds its target, so the retry carries it on rather than leaving the
   *  reader to find the row themselves. */
  async function retryMore(): Promise<void> {
    const how = await loadMoreThread();
    if (how !== "page") return;
    const target = walkTarget;
    const root = rootOf();
    if (target === null || root === null) return;
    if (typeof target === "number") {
      await walkAndLand(root, target);
      return;
    }
    await walkTo(target);
  }

  /** Walk down the open discussion until the target is drawn, or until the
   *  discussion ends. A quote jump and the way back to the newest reply share
   *  this walk and its page cap; a walk that stops at the cap keeps its target,
   *  so the reader can carry on from where it stopped. */
  async function walkTo(target: number | "newest"): Promise<WalkOutcome> {
    const root = rootOf();
    if (root === null || read === null) return "gone";
    const room = roomId();
    const gen = generation;
    walkTarget = target;
    walkStopped = false;
    walkMissing = false;
    // Found is a row the window draws, not a page it fetched: the window draws
    // the room's rows inside the stretch it read as well as the pages, so a row
    // can be on screen while no page returned it. A position the window has
    // passed is not the same fact.
    const drawn = (): boolean =>
      target !== "newest" && lines.some((line) => line.seq === target);
    let taken = 0;
    // A page already in flight means a fetch of ours returns at once having
    // taken nothing; the walk waits for that page rather than spending budget
    // on it, and a wait that never settles leaves the walk undecided rather
    // than spinning or calling the row absent.
    let waits = 0;
    let undecided = false;
    for (;;) {
      if (drawn()) break;
      const held = read;
      if (held === null || !hasMore) break;
      if (taken >= THREAD_WALK_MAX_PAGES) {
        walkStopped = true;
        return "capped";
      }
      // Only a page that is actually being read gives the reader anything to
      // follow: a walk that finds its row here moves no scroll at all.
      let how: "page" | "none" | "failed";
      try {
        walking = true;
        how = await loadMoreThread();
      } finally {
        walking = false;
      }
      if (!holds(gen, room, root)) return "gone";
      if (how === "failed") return "failed";
      if (how === "none") {
        if (waits >= WALK_WAIT_LIMIT) {
          undecided = true;
          break;
        }
        waits += 1;
        await new Promise((resolve) => setTimeout(resolve, 25));
        continue;
      }
      taken += 1;
    }
    if (drawn()) {
      walkTarget = null;
      return "found";
    }
    if (undecided) {
      // The page the walk waited on never settled, so the discussion's end was
      // not read: nothing here says the row is absent. The target stays, and
      // the foot offers the way on that carries it.
      const next = hasMore;
      walkStopped = next;
      moreFailed = !next;
      return next ? "capped" : "failed";
    }
    walkTarget = null;
    if (target === "newest") return "end";
    // The discussion ended without drawing the row: no further page can. This
    // is also where a spent read ends the walk: the gate is shut, so the row is
    // one no page brings rather than one that was never here, and the foot's
    // words for it are only approximate. A branch of its own would be rarer
    // than the sentence costs; see `threadPageClaim`.
    walkMissing = true;
    return "end";
  }

  /** Walk down the open discussion and land on the row that was asked for. A
   *  quote takes this path whether it opens the discussion or carries on a walk
   *  that stopped at the page cap: reaching the row and stopping there belong
   *  together, or the reader is left at the bottom of what was read with the
   *  row they asked for somewhere above them. */
  async function walkAndLand(root: number, target: number): Promise<void> {
    const gen = generation;
    const how = await walkTo(target);
    if (how !== "found") return;
    await tick();
    if (gen === generation && rootOf() === root) onLanded?.(target);
  }

  /** Carry on a walk that stopped at the page cap. */
  async function continueWalk(): Promise<void> {
    const target = walkTarget;
    const root = rootOf();
    if (target === null || root === null) return;
    walkStopped = false;
    if (typeof target === "number") {
      await walkAndLand(root, target);
      return;
    }
    await walkTo(target);
  }

  /** Send one reply into the open discussion, then walk to the newest row so
   *  the confirmed echo takes the pending line's place. A rejection goes to the
   *  surface, which keeps the reader's text and shows the shared failure
   *  line. */
  async function sendReply(text: string, quote?: number): Promise<void> {
    const room = roomId();
    const root = rootOf();
    const gen = generation;
    if (root === null) return;
    await roomConversationsStore.send(room, text, { thread: root, quote });
    if (!holds(gen, room, root)) return;
    await walkTo("newest");
  }

  // The discussion this window was last synced to, as one value: a surface may
  // report the same root on every effect run, and only a change is a read.
  let synced: string | null = null;

  /** Everything the window holds, back to nothing -- and the pair it was synced
   *  to forgotten with it, so a surface that closes a discussion it opened
   *  itself reads it again on the next sync instead of sitting on an empty
   *  window whose pair still counts as synced. */
  function clear(): void {
    synced = null;
    reset();
  }

  /** Follow the surface's room and root: a change (a close included) clears the
   *  window, and a new discussion is read from its first page. The same pair
   *  does nothing, so a surface calls this on every effect run. A surface that
   *  opens discussions itself calls `openThread` instead. */
  function sync(): void {
    const room = roomId();
    const root = rootOf();
    const next = root === null ? null : `${room}:${root}`;
    if (next === synced) return;
    // The reset first, the pair second: `reset` is the state alone, so recording
    // the pair after it is what makes the next run of this effect a no-op. A
    // `clear` here would forget the pair and read again on every run.
    reset();
    synced = next;
    if (root !== null) void openThread(root);
  }

  // Getters, not values: Svelte compiles a shorthand property in a returned
  // object to one `$.get` at the moment the factory runs, which freezes every
  // cell at its init value (an empty window, a room still undefined). A getter
  // reads the cell where the surface reads it.
  return {
    get read() {
      return read;
    },
    get pages() {
      return pages;
    },
    get loading() {
      return loading;
    },
    get unreadable() {
      return unreadable;
    },
    get loadingMore() {
      return loadingMore;
    },
    get moreFailed() {
      return moreFailed;
    },
    get walking() {
      return walking;
    },
    get walkStopped() {
      return walkStopped;
    },
    get walkMissing() {
      return walkMissing;
    },
    get spentClaim() {
      return spentClaim;
    },
    get covered() {
      return covered;
    },
    get beyond() {
      return beyond;
    },
    get pagingNow() {
      return pagingNow;
    },
    get more() {
      return more;
    },
    get lines() {
      return lines;
    },
    get hasMore() {
      return hasMore;
    },
    get isComplete() {
      return isComplete;
    },
    get membership() {
      return membership;
    },
    sync,
    clear,
    openThread,
    retryThread,
    loadMoreThread,
    retryMore,
    walkTo,
    walkAndLand,
    continueWalk,
    sendReply,
    recordSpentClaim,
  };
}
