<script lang="ts">
  // One room's conversation: a member-aware transcript rendered from the
  // lesche's payload store + an outbound composer that posts plaintext. Rooms
  // bypass the bilateral projector (no channelsStore / chat_history), so this
  // is a separate page from ChannelChatPage -- the transcript model is
  // multi-member (a sender uuid per line, not a user/assistant role).
  //
  // The page composes the shared chrome (PageHeader top bar + its own
  // loading/error/empty states) and the shared <Composer>. The transcript
  // renders inline (member-
  // aware: `mine` alignment + a sender label for others); stick-to-tail auto-
  // scroll is the shared `createAutoScroll`. A poll pump refreshes the history
  // + roster on a slow cadence (the room_membership_changed SSE nudge is the
  // fast path; the poll is the dropped-frame backstop).
  import { onMount, tick, untrack } from "svelte";
  import { Settings, Users, X } from "@lucide/svelte";
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import Composer from "../components/Composer.svelte";
  import MessageBubble from "../components/MessageBubble.svelte";
  import DeleteFailedButton from "../components/DeleteFailedButton.svelte";
  import PageHeader from "../components/PageHeader.svelte";
  import ScrollToBottomButton from "../components/ScrollToBottomButton.svelte";
  import {
    createAutoScroll,
    createTogglePin,
  } from "../lib/transcript.svelte.ts";
  import type { ArmedQuote } from "../lib/quote.ts";
  import {
    armedQuoteSeq,
    previewOf,
    roomQuoteBlocksByLine,
  } from "../lib/quote.ts";
  import { createReveal, revealMark } from "../lib/reveal.svelte.ts";
  import { ROW_TIME } from "../lib/time/stamp.ts";
  import { createComposer } from "../lib/composer.svelte.ts";
  import { createMentionPicker } from "../lib/mentionPicker.svelte.ts";
  import { mentionOptionId } from "../lib/mentions.ts";
  import { bindDraft } from "../lib/session/drafts.svelte.ts";
  import { roomDraftKey } from "../lib/session/drafts.ts";
  import {
    mentionChipsOf,
    type RoomLine,
    roomRowAnnouncement,
    roomLinePending,
    roomMessageLabel,
    roomRowAnchorId,
    roomThreadRowAnchorId,
  } from "../lib/room-message.ts";
  import MemberRow from "../components/rooms/MemberRow.svelte";
  import MentionPicker from "../components/rooms/MentionPicker.svelte";
  import MentionChips from "../components/MentionChips.svelte";
  import MessageRow from "../components/MessageRow.svelte";
  import QuoteButton from "../components/QuoteButton.svelte";
  import QuoteBar from "../components/QuoteBar.svelte";
  import SenderIdentity from "../components/rooms/SenderIdentity.svelte";
  import ThreadPanel from "../components/rooms/ThreadPanel.svelte";
  import {
    roomConversationsStore,
    THREAD_WALK_MAX_PAGES,
  } from "../lib/session/roomConversations.svelte.ts";
  import { roomsStore } from "../lib/session/rooms.svelte";
  import { archeionSession } from "../lib/session/archeion.svelte";
  import { unreadStore, roomKey } from "../lib/session/unread.svelte.ts";
  import { navigate } from "../lib/shell/port.ts";
  import { profileHref } from "../lib/room-message.ts";
  import { startVisibleInterval } from "../lib/visibleInterval.ts";
  import { TONAL_ICON_SURF } from "../lib/classes.ts";
  import {
    appendThreadPage,
    drawnRootState,
    threadAtEnd,
    threadEntry,
    threadGroups,
    threadMembership,
    threadRoot,
    threadRows,
    type ThreadPage,
    type ThreadMore,
    type ThreadRootState,
    threadWindowEnd,
    threadPaging,
    threadPageClaim,
    threadRoomBeyondCount,
  } from "../lib/threads.ts";
  import { lgQuery } from "../lib/shell/breakpoint.ts";
  import { formatStamp } from "../lib/time/stamp.svelte.ts";
  import {
    common_loading,
    common_retry,
    room_title,
    room_label_fallback,
    room_member_one,
    room_member_other,
    room_id_label,
    room_public_badge,
    room_creator_badge,
    room_toggle_members_aria,
    room_settings_aria,
    room_loading_history,
    room_empty,
    room_sending_aria,
    room_members_aria,
    room_members,
    room_close_members_aria,
    room_unavailable,
    nav_rooms,
    chat_transcript_aria,
    room_thread_last_from,
    room_thread_orphan_note,
    room_thread_pending_note,
    room_thread_panel_title,
    room_thread_sheet_description,
    room_thread_replies_one,
    room_thread_replies_other,
    room_thread_view,
  } from "../paraglide/messages.js";

  let { roomId }: { roomId: string } = $props();

  const conv = $derived(roomConversationsStore.get(roomId));
  // The registry view for this room (name/description/visibility), for the
  // header title. Null only if the room has not surfaced in the registry yet (a
  // brief resolving window).
  const room = $derived(
    roomsStore.rooms.find((r) => r.room_id === roomId) ?? null,
  );
  const roomLabel = $derived(
    room?.name || room_label_fallback({ id: roomId.slice(0, 8) }),
  );

  // The quote block each line draws, resolved once per render over the window
  // already on screen. The room transcript keeps every line it has loaded (a
  // history page is capped at 200 rows, but the store appends pages and live
  // frames without trimming), so a lookup per row would be quadratic in it.
  const quotes = $derived(roomQuoteBlocksByLine(conv?.lines ?? []));

  // Each discussion the transcript draws, one entry per head. A reply whose
  // root the loaded window does not hold still gets an entry (see lib/threads.ts):
  // a row that vanishes from a transcript reads as a message never sent.
  const groups = $derived(threadGroups(conv?.lines ?? []));
  // A reply count is a number only for a discussion whose head is loaded and
  // only once the transcript has reached the room's present: replies sit after
  // their head, so a window that reaches the present holds all of them, while
  // a window still walking forward has passed only some of them.
  const countsKnown = $derived(conv?.caughtUp ?? false);

  // The roster keyed by participant id: one pass serves both the sender label
  // the bubble header draws and the members a line mentions. The wire
  // `Participant` on each message carries the sender's kind + server-resolved
  // handle (the lesche derives it fresh at read time, so it is already
  // correct) but not the mutable display label, so the header reads that label
  // here; a mention carries ids alone, so the chip resolves label/handle/href
  // here too. Undefined for a sender not in the roster / before the roster
  // lands (handle alone); rebuilt with the roster, so a rename re-renders both.
  const memberById = $derived(
    new Map((conv?.roster?.members ?? []).map((m) => [m.id, m] as const)),
  );

  // The discussion the panel is reading, and the stretch of it that has landed.
  // It all belongs to one room: the page instance survives a room-id change
  // (the route keys on the pattern, not the id), so the room reset effect
  // closes it.
  let openThreadRoot = $state<number | null>(null);
  let threadRead = $state<ThreadPage | null>(null);
  // How many pages of the open discussion have landed, the first one included:
  // the panel tells its own fetches from rows the room brings in, and only the
  // latter are followed.
  let threadPages = $state(0);
  let threadLoading = $state(false);
  let threadUnreadable = $state(false);
  let threadLoadingMore = $state(false);
  let threadMoreFailed = $state(false);
  // One walk down the discussion toward a row the panel does not hold yet. The
  // target outlives a walk that stopped at the page cap, so the reader can ask
  // for the next stretch from where it stopped.
  let threadWalkTarget = $state<number | "newest" | null>(null);
  // A walk is in flight: the panel follows it down, so the reader sees the
  // pages arrive rather than a count off screen.
  let threadWalking = $state(false);
  let threadWalkStopped = $state(false);
  let threadWalkMissing = $state(false);
  // Rows of this discussion the room holds past the read that no page will
  // bring, counted as they stood when a short page answered for them. That
  // count is the key a verdict is taken back on: rows the room takes since
  // are a fresh claim and earn one more page, where a latch would keep them
  // out of the panel until the discussion was opened again.
  let threadSpentClaim = $state<number | null>(null);

  // What the foot of the panel says, from the state that makes it true.
  /** How a walk down a discussion ended: on the row it was sent for, at the
   *  discussion's end without drawing that row, at the page cap, on a page
   *  that failed, or on a panel that is no longer the one that asked. Only
   *  `found` lands on a row; `failed` keeps its target, so the panel's retry
   *  can carry the walk on. */
  type WalkOutcome = "found" | "end" | "capped" | "failed" | "gone";

  /** How many 25ms waits a walk gives a page already in flight before it
   *  gives up: ten seconds, well past a page that is merely slow. */
  const WALK_WAIT_LIMIT = 400;
  // The end of the stretch the panel may draw: the pages' last cursor carried
  // forward over the room rows already held. The transcript and the paging
  // gate both start from it, so it is worked out once.
  const threadCovered = $derived(
    threadWindowEnd(threadRead?.lastSeq ?? null, conv?.lines ?? []),
  );

  // How many rows of this discussion the room places past that stretch: a
  // page still worth asking for, until a short page answers for them.
  const threadBeyond = $derived(
    openThreadRoot === null
      ? 0
      : threadRoomBeyondCount(openThreadRoot, threadCovered, conv?.lines ?? []),
  );

  // What the open read makes of the paging gate and the foot, from the rows
  // the room places past the stretch and the count a short page answered for.
  const threadPagingNow = $derived(
    threadPaging(threadRead, threadBeyond, threadSpentClaim),
  );
  const threadMore = $derived.by((): ThreadMore => {
    if (threadWalkMissing) return "missing";
    if (threadWalkStopped) return "stopped";
    if (threadMoreFailed) return "failed";
    if (threadLoadingMore) return "loading";
    if (threadPagingNow.spent) return "unreachable";
    return "idle";
  });

  /** Everything the panel holds of a discussion, back to nothing. Opening
   *  another discussion and closing this one differ only in whether a root
   *  is left behind, so the state is cleared in one place. */
  function clearThreadWindow(): void {
    threadRead = null;
    threadPages = 0;
    threadLoading = false;
    threadUnreadable = false;
    threadLoadingMore = false;
    threadMoreFailed = false;
    threadSpentClaim = null;
    threadWalkTarget = null;
    threadWalkStopped = false;
    threadWalkMissing = false;
    threadWalking = false;
  }

  /** Start the panel on a discussion with nothing read yet. */
  function resetThreadWindow(root: number): void {
    openThreadRoot = root;
    clearThreadWindow();
  }

  /** Read one page and keep it only if the panel on screen asked for it: a
   *  page that lands after another discussion opened answers a panel that is
   *  gone, and drawing it would put one discussion's rows under another's
   *  head. The room is captured with the root: a page must never land in the
   *  room next door. */
  async function readThreadPage(
    room: string,
    root: number,
    afterSeq: number,
  ): Promise<ThreadPage | null> {
    const page = await roomConversationsStore.fetchThreadPage(
      room,
      root,
      afterSeq,
    );
    return roomId === room && openThreadRoot === root ? page : null;
  }

  /** Record what a landing page claims, from the count taken before the ask:
   *  both fetches measure the rows past the stretch first and hand that count
   *  here, so a reply that arrives while the page is in flight is not swallowed
   *  as one the page answered for. See `threadPageClaim`. */
  function recordSpentClaim(
    page: { readonly hasMore: boolean },
    beyondBefore: number,
  ): void {
    threadSpentClaim = threadPageClaim(page, beyondBefore);
  }

  /** Open one discussion: the panel shows its loading state at once, then
   *  the page the root sits on lands and the reader starts at its top. The
   *  root's own page is asked for by position, so a discussion opens in one
   *  round trip however long it is. */
  async function openThread(root: number): Promise<void> {
    const room = roomId;
    resetThreadWindow(root);
    threadLoading = true;
    try {
      const page = await readThreadPage(room, root, root - 1);
      if (page !== null) {
        threadRead = page;
        threadPages += 1;
        // A first page has no drawn stretch before it, so no row it could have
        // answered for: it claims nothing.
        recordSpentClaim(page, 0);
      }
    } catch (e) {
      console.error("[room thread] first page failed:", e);
      if (roomId === room && openThreadRoot === root) {
        threadUnreadable = true;
      }
    } finally {
      if (roomId === room && openThreadRoot === root) {
        threadLoading = false;
      }
    }
  }

  /** Ask again for a first page the panel could not read. */
  async function retryThread(): Promise<void> {
    const root = openThreadRoot;
    if (root === null) return;
    await openThread(root);
  }

  /** Take the page below what the panel holds. One page per call; the read's
   *  `hasMore` says whether another exists. `walkTo` and the panel's own
   *  scrolling both come through here, and both need to know whether a page
   *  was actually taken: a call that returns while another page is in flight
   *  fetched nothing, and a walk that counted it would spend its budget on
   *  pages it never asked for. */
  async function loadMoreThread(): Promise<"page" | "none" | "failed"> {
    const root = openThreadRoot;
    const read = threadRead;
    if (root === null || read === null) return "none";
    if (threadLoadingMore) return "none";
    // The last page can be short and the discussion still be behind the room:
    // a reply taken since sits past the stretch those pages drew.
    if (!threadHasMore) return "none";
    const room = roomId;
    const cursor = read.lastSeq ?? root - 1;
    // The rows of this discussion the room shows past the stretch before the
    // ask: a short page under them is the server's last word for this cursor,
    // and this count is what that verdict is taken back on. Measured here, so
    // a reply taken while the page is in flight is not claimed as answered.
    const beyondBefore = threadRoomBeyondCount(
      root,
      threadCovered,
      conv?.lines ?? [],
    );
    threadLoadingMore = true;
    threadMoreFailed = false;
    try {
      const page = await readThreadPage(room, root, cursor);
      if (page === null) return "none";
      const next = appendThreadPage(threadRead, page);
      threadRead = next;
      recordSpentClaim(next, beyondBefore);
      threadPages += 1;
      // A page landed, so whatever the foot said about where the window
      // stopped is out of date.
      threadWalkStopped = false;
      return "page";
    } catch (e) {
      console.error("[room thread] next page failed:", e);
      if (roomId === room && openThreadRoot === root) {
        threadMoreFailed = true;
      }
      return "failed";
    } finally {
      if (roomId === room && openThreadRoot === root) {
        threadLoadingMore = false;
      }
    }
  }

  /** The panel's retry after a page failed. A walk that ran into the
   *  failure still holds its target, so the retry carries it on rather than
   *  leaving the reader to find the row themselves. */
  async function retryMore(): Promise<void> {
    const how = await loadMoreThread();
    if (how !== "page") return;
    const target = threadWalkTarget;
    const root = openThreadRoot;
    if (target === null || root === null) return;
    if (typeof target === "number") {
      await walkAndLand(root, target);
      return;
    }
    await walkTo(target);
  }

  /** Walk down the open discussion until the target is drawn, or until the
   *  discussion ends. A quote jump and the way back to the newest reply share
   *  this walk and its page cap; a walk that stops at the cap keeps its
   *  target, so the reader can carry on from where it stopped. */
  async function walkTo(target: number | "newest"): Promise<WalkOutcome> {
    const root = openThreadRoot;
    if (root === null || threadRead === null) return "gone";
    const room = roomId;
    threadWalkTarget = target;
    threadWalkStopped = false;
    threadWalkMissing = false;
    // Found is a row the panel draws, not a page it fetched: the panel draws
    // the room's rows inside the stretch it read as well as the pages (see
    // threadRows), so a row can be on screen while no page returned it. A
    // position the window has passed is not the same fact.
    const drawn = (): boolean =>
      target !== "newest" && threadLines.some((line) => line.seq === target);
    let pages = 0;
    // A page already in flight means a fetch of ours returns at once
    // having taken nothing; the walk waits for that page rather than
    // spending budget on it, and a wait that never settles leaves the walk
    // undecided rather than spinning or calling the row absent.
    let waits = 0;
    let undecided = false;
    for (;;) {
      if (drawn()) break;
      const read = threadRead;
      if (read === null || !threadHasMore) break;
      if (pages >= THREAD_WALK_MAX_PAGES) {
        threadWalkStopped = true;
        return "capped";
      }
      // Only a page that is actually being read gives the reader anything
      // to follow: a walk that finds its row here moves no scroll at all.
      let how: "page" | "none" | "failed";
      try {
        threadWalking = true;
        how = await loadMoreThread();
      } finally {
        threadWalking = false;
      }
      if (roomId !== room || openThreadRoot !== root) return "gone";
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
      pages += 1;
    }
    if (drawn()) {
      threadWalkTarget = null;
      return "found";
    }
    if (undecided) {
      // The page the walk waited on never settled, so the discussion's
      // end was not read: nothing here says the row is absent. The
      // target stays, and the foot offers the way on that carries it.
      const more = threadHasMore;
      threadWalkStopped = more;
      threadMoreFailed = !more;
      return more ? "capped" : "failed";
    }
    threadWalkTarget = null;
    if (target === "newest") return "end";
    // The discussion ended without drawing the row: no further page can.
    // This is also where a spent read ends the walk: the gate is shut, so the
    // row is one no page brings rather than one that was never here, and the
    // foot's words for it are only approximate. A branch of its own would be
    // rarer than the sentence costs; see `threadPageClaim`.
    threadWalkMissing = true;
    return "end";
  }

  /** Walk down the open discussion and land on the row that was asked for.
   *  A quote takes this path whether it opens the discussion or carries on a
   *  walk that stopped at the page cap: reaching the row and stopping there
   *  belong together, or the reader is left at the bottom of what was read
   *  with the row they asked for somewhere above them. */
  async function walkAndLand(root: number, target: number): Promise<void> {
    const how = await walkTo(target);
    if (how !== "found") return;
    await tick();
    if (openThreadRoot === root) threadReveal.jump(target);
  }

  /** Carry on a walk that stopped at the page cap. */
  async function continueWalk(): Promise<void> {
    const target = threadWalkTarget;
    const root = openThreadRoot;
    if (target === null || root === null) return;
    threadWalkStopped = false;
    if (typeof target === "number") {
      await walkAndLand(root, target);
      return;
    }
    await walkTo(target);
  }

  /** Open a discussion and then put the reader on one of its rows: a quote
   *  can name a reply the transcript never drew, so its jump opens the
   *  discussion that holds it and walks down to that row rather than asking
   *  this transcript for a missing row. */
  async function openThreadAt(root: number, seq: number): Promise<void> {
    await openThread(root);
    if (openThreadRoot !== root) return;
    await walkAndLand(root, seq);
  }

  /** Send one reply into the open discussion, then walk to the newest row so
   *  the confirmed echo takes the pending line's place. A rejection goes to
   *  panel, which keeps the reader's text and shows the shared failure
   *  line. */
  async function sendReply(text: string): Promise<void> {
    const room = roomId;
    const root = openThreadRoot;
    if (root === null) return;
    await roomConversationsStore.send(room, text, { thread: root });
    if (roomId !== room || openThreadRoot !== root) return;
    await walkTo("newest");
  }

  /** The panel's way back to the discussion's newest reply: the same walk a
   *  reply takes, so it stops at the same page cap. */
  async function returnToNewest(): Promise<void> {
    if (openThreadRoot === null) return;
    await walkTo("newest");
  }

  /** Close the panel: the discussion leaves, and so does everything read of
   *  it, so opening it again starts at its root page. */
  function closeThread(): void {
    openThreadRoot = null;
    clearThreadWindow();
  }

  // The rows the panel draws: the pages read so far, plus the room's own rows
  // of this discussion inside the stretch those pages cover, which a read may
  // not return (an own pending send; a reply whose declared head was missing
  // when it was written, and so stored no root). `threadCovered` carries the
  // stretch past the last page over rows the room already holds, so a reply
  // that lands after the read still shows.
  const threadLines = $derived(
    openThreadRoot === null
      ? []
      : threadRows(
          openThreadRoot,
          conv?.lines ?? [],
          threadRead?.lines ?? [],
          threadCovered,
        ),
  );

  // The panel asks for a page while the read says more, and while the room
  // holds a row of this discussion past the stretch its pages drew; it stops
  // once a short page has answered for exactly those rows, because no page
  // starts below them. See `threadPaging`.
  const threadHasMore = $derived(threadPagingNow.more);

  // Whether the panel has read the discussion to its end. A window that stops
  // on a full page, or one the room still holds rows of, may not present
  // itself as the whole: the gate closing is not the same fact as the read
  // having drawn every row.
  const threadIsComplete = $derived(
    threadAtEnd(threadRead) && threadBeyond === 0,
  );

  // Where the open discussion's root sits relative to the loaded window, read
  // from the same grouping the transcript draws: the entry and the panel header
  // must never say different things about the same discussion. A root the
  // panel draws itself is rooted whatever the window holds: the discussion read
  // reaches roots the room's loaded window never had (see `drawnRootState`).
  const openThreadState = $derived<ThreadRootState>(
    openThreadRoot === null
      ? "unknown"
      : drawnRootState(
          openThreadRoot,
          threadLines,
          groups.find((group) => group.root === openThreadRoot)?.rootState,
        ),
  );

  // The note under a discussion's entry follows the panel while that discussion
  // is open: the panel draws rows the room's grouping cannot see, so without
  // this an entry saying the discussion starts before the loaded window would
  // sit beside a header showing its head. The count and the last sender stay
  // the grouping's.
  const entryRootStates = $derived(
    new Map(
      groups.map((group) => [
        group.root,
        group.root === openThreadRoot ? openThreadState : group.rootState,
      ]),
    ),
  );

  // The panel's own row table: it draws replies the transcript does not, so a
  // quote whose target is a reply lands here.
  const threadReveal = createReveal({
    scope: () =>
      openThreadRoot === null ? undefined : `${roomId}:${openThreadRoot}`,
    describe: (seq) => {
      const line = threadLines.find((row) => row.seq === seq);
      if (line === undefined) return undefined;
      return roomRowAnnouncement(line, {
        time: formatStamp(line.createdAt, ROW_TIME),
        authorLabel: memberById.get(line.senderId)?.label,
      });
    },
  });

  // Why the reader is in the open discussion, as the panel header says it:
  // null when the read could not see the whole discussion, so a half
  // discussion never explains the wrong reason.
  const membership = $derived(
    openThreadRoot === null
      ? null
      : threadMembership(
          openThreadRoot,
          threadLines,
          archeionSession.participantId,
          threadIsComplete,
        ),
  );

  /** The discussion a quote target belongs to: a head is its own, a reply is
   *  filed under another row. */
  function quotedRoot(seq: number): number | undefined {
    const line = conv?.lines.find((row) => row.seq === seq);
    return line === undefined ? undefined : threadRoot(line);
  }

  /** A quote block's jump. A head is drawn here, so scroll to it; a reply is
   *  drawn only in the panel, so open the discussion that holds it. */
  function jumpToQuote(seq: number): void {
    const root = quotedRoot(seq);
    if (root === undefined) return;
    if (root === seq) {
      reveal.jump(seq);
      return;
    }
    void openThreadAt(root, seq);
  }

  /** The id that describes a quote block: the target row's own hidden label,
   *  and only while this view draws that row. A reply's label lives in the
   *  panel, so the block points there while that panel is open; a closed panel
   *  would leave a dangling id. */
  function quoteAnchor(seq: number): string | undefined {
    const root = quotedRoot(seq);
    if (root === undefined) return undefined;
    if (root === seq) return roomRowAnchorId(seq);
    return openThreadRoot === root ? roomThreadRowAnchorId(seq) : undefined;
  }

  // Width decides where the panel lives: a column beside the room on lg+, a
  // full-screen sheet below it. The listener re-evaluates on resize and is
  // removed with the effect.
  const lgMql = matchMedia(lgQuery);
  let wide = $state(lgMql.matches);
  $effect(() => {
    const onChange = (event: MediaQueryListEvent) => (wide = event.matches);
    lgMql.addEventListener("change", onChange);
    return () => lgMql.removeEventListener("change", onChange);
  });

  // Whether the roster has landed -- the chip draws the bare short id until
  // it does.
  const rosterLoaded = $derived(conv?.roster != null);

  // Open the conversation (loads the history). Driven from an effect (not a
  // one-shot onMount) gated on the self participant id (needed to flag own
  // messages -- `mine`). Otherwise a refresh races open() ahead of it resolving
  // and bakes wrong `mine` flags into the rendered lines. open() is idempotent
  // and re-opens from an error state, so re-firing once ready recovers it.
  // Refresh the roster alongside for an at-once member count.
  //
  // The store calls are commands keyed on the participant id, not reactive
  // derivations, so they run inside `untrack`: enter() reads the very entry
  // fields it resets, and the cleanup's leaveRoom() advances them -- tracked,
  // every unread-store write re-fired this effect, and with two instances of
  // the page mounted the interleaved cleanup writes drove the knownSeq
  // ping-pong into effect_update_depth_exceeded (operator live log 2026-09-01).
  $effect(() => {
    if (!archeionSession.participantId) return;
    untrack(() => {
      void roomConversationsStore.open(roomId);
      // Viewing: an explicitly opened room page clears its badge
      // and starts the cursor-write schedule; the cleanup (unmount or a
      // participantId re-fire) syncs the watermark to the conversation's live
      // cursor and flushes the write.
      unreadStore.enter(roomKey(roomId));
      void roomConversationsStore.refreshRoster(roomId);
    });
    return () => untrack(() => unreadStore.leaveRoom(roomId, conv?.lastSeq));
  });

  // Slow poll: the dropped-frame backstop for the room_membership_changed
  // roster nudge + offline-member catch-up. Bounded so a long-idle open tab
  // does not spin. Cleared on unmount.
  onMount(() => {
    const stop = startVisibleInterval(() => {
      // A terminal error (e.g. this member was removed) stops the poll: the
      // room is gone to this user, and refreshing would only loop the failure.
      if (conv?.status === "error") return;
      void roomConversationsStore.refresh(roomId);
      // Refresh the roster (member count + creator badge) at the same cadence;
      // the room_membership_changed SSE is the faster trigger.
      void roomConversationsStore.refreshRoster(roomId);
    }, 10_000);
    return () => stop();
  });

  // Stick to the tail as lines arrive; stop once the user scrolls up to read.
  const scroll = createAutoScroll();

  /** The composer instance, for the case where no row is left to land on:
   *  an emptied transcript after the last line is discarded. */
  let composerRef: { focusInput(): boolean } | null = $state(null);

  /** Run the discard, then put focus where the reader continues: the row above
   *  the one that left, else the composer, else the transcript itself. The
   *  thread place is read before the discard runs: the store drops the row as
   *  it answers, so a lookup afterwards names nothing and sends the reader to
   *  the top. The place is a discussion's, not a row's, because the transcript
   *  draws one entry per discussion and only a head can carry a failed send. */
  function discardRow(removed: number): boolean {
    const at = groups.findIndex((group) => group.head.seq === removed);
    if (!roomConversationsStore.discard(roomId, removed)) return false;
    landFocus(at > 0 ? (groups[at - 1]?.head.seq ?? null) : null);
    return true;
  }

  /** Drop a failed reply from the panel: the store's reducer alone, because
   *  the panel keeps its own focus (the transcript behind it is not where the
   *  reader is). */
  function discardReplyLine(seq: number): boolean {
    return roomConversationsStore.discard(roomId, seq);
  }

  /** Focus the row `id`, or the next thing that can speak for the transcript:
   *  the composer, then the viewport itself. A disabled composer cannot take
   *  focus, and focus must not fall through to the document. */
  function landFocus(id: number | null): void {
    if (id !== null && reveal.land(id)) return;
    if (composerRef?.focusInput() !== true) {
      scroll.viewport?.focus({ preventScroll: true });
    }
  }
  // Session-key reset must be declared BEFORE the stick effect: effects in
  // the same flush run in declaration order, so a param change resets the
  // controllers before stick() observes the new room's lines. A pending
  // quote is not cleared here: the bar reads it through armedQuoteSeq, which
  // compares scopes only, so a seq armed in the room just left is unreadable
  // here while the state stays: returning to that room shows the strip again,
  // and cancelling or sending are the two paths that clear it. The sentence
  // the reveal spoke goes with them, from this effect's cleanup: destroy()
  // reads back the spoken slot it wrote to flip it, and a tracked body would
  // register that read and dirty itself; a cleanup has no reaction to record.
  $effect(() => {
    void roomId;
    // The page instance survives a room-id change (the route keys on the
    // pattern, not the id), so the discussion panel closes with the room it was
    // opened in: its root numbers rows of a room no longer on screen.
    closeThread();
    scroll.reset();
    return () => {
      reveal.destroy();
      threadReveal.destroy();
    };
  });
  $effect(() => {
    void conv?.lines.length;
    scroll.stick(conv?.lines.length ?? 0, conv?.lines.at(-1)?.seq);
  });
  // Layout-driven resizes (composer auto-grow, window resize) shrink the
  // container without a scroll event; while following, re-anchor to the
  // tail so a send never visually drags it out of view.
  $effect(() => {
    const vp = scroll.viewport;
    if (!vp) return;
    return scroll.observe(vp);
  });

  // The viewing line tick: lines rendered on the open conversation count as
  // read and coalesce the cursor write into the 5s throttle window: a
  // busy chat must not POST per line; the leave path flushes.
  //
  // Only the rendered-line count is the trigger. noteViewedLines runs inside
  // `untrack`: its internal knownSeq fence reads the same entry field the
  // call may write, and a tracked read there let store writes re-invoke this
  // effect mid-flush (the write/read ping-pong with the mount effect's
  // cleanup above). Lines and lastSeq advance together, so the count alone
  // is a sound tick.
  $effect(() => {
    if (conv?.lines.length === undefined) return;
    untrack(() => {
      unreadStore.noteViewedLines(roomId, conv?.lastSeq ?? null);
    });
  });

  // One scroll-pin controller for the whole room transcript (a single active
  // ResizeObserver across all bubbles); each <MessageBubble> hands its box +
  // action row to it on a raw toggle.
  const togglePin = createTogglePin(() => scroll.viewport);

  // Jumping to the row a quote names: the same row table, scroll and brief
  // mark the 1:1 transcript uses, keyed by this room's seq.
  const reveal = createReveal({
    scope: () => roomId,
    // A jump moves focus without a word, so the controller speaks the row it
    // landed on: the author and time that row's own label names.
    describe: (seq) => {
      const line = conv?.lines.find((row) => row.seq === seq);
      if (line === undefined) return undefined;
      return roomRowAnnouncement(line, {
        time: formatStamp(line.createdAt, ROW_TIME),
        authorLabel: memberById.get(line.senderId)?.label,
      });
    },
  });

  const disabled = $derived(!conv?.historyLoaded);

  // The togglable member side-panel (online dots driven by the room's live
  // online-member set + roster identity).
  let showMembers = $state(false);

  // The row this send answers, tagged with the room that armed it: one quote
  // at a time, replaced by quoting another row and spent by sending or Escape.
  let pendingQuote = $state<ArmedQuote | null>(null);

  // Reads go through one gate: a seq armed in the room just left numbers a
  // different row here, so the bar, the preview and the send see undefined.
  const armedSeq = $derived(armedQuoteSeq(pendingQuote, roomId));

  // Only a row the room has numbered can be quoted: an optimistic line has no
  // seq the other side could resolve, and a failed one is about to be resent as
  // a different line.
  function quotable(line: RoomLine): boolean {
    return line.seq > 0 && !roomLinePending(line) && !line.failed;
  }
  // A flash timer outliving the transcript would touch a torn-down view:
  // the final teardown runs the reveal's own cleanup -- its mark and
  // sentence with the timer -- and a room swap drops the sentence from its
  // own effect's cleanup, its body resetting the mark.
  $effect(() => () => {
    reveal.destroy();
  });

  const composer = createComposer({
    send: (text) => {
      // One send spends the strips: the picks and the quote are captured here
      // and cleared before the transport await, so a second submit cannot
      // inherit what the user already spent. A failed send keeps both on the
      // optimistic line, which is where the retry reads them back.
      const mentions = picker.picked;
      const quote = armedSeq;
      picker.clear();
      pendingQuote = null;
      return roomConversationsStore.send(roomId, text, { mentions, quote });
    },
    // Busy is not a gate: send renders the optimistic line at once + posts.
    canSubmit: () => conv?.historyLoaded ?? false,
  });

  // Draft storage: rooms key on the room id.
  bindDraft(composer, () => roomDraftKey(roomId));

  // The mention picker: the page holds its state, lib/mentions.ts holds every
  // rule it applies. Candidates are the roster already loaded for the chips --
  // no second fetch, and no popup before the roster lands (there would be
  // nothing to offer, and a half-state popup is worse than none).
  const picker = createMentionPicker({
    // The picks are the composer's, so a swap keeps each room's own and the
    // reader shows the live one: coming back restores what was picked here.
    scope: () => roomId,
    members: () => conv?.roster?.members ?? [],
    draft: () => composer.draft,
    caret: () => composer.caret,
    dropToken: (start, end) => {
      const draft = composer.draft;
      composer.draft = draft.slice(0, start) + draft.slice(end);
      composer.requestCaret(start);
    },
  });

  // The picked members as the same chips a message draws: one vocabulary, one
  // degradation rule. A member who has left the roster keeps the chip the user
  // picked -- it falls back to the roster-missing shape rather than vanishing.
  const pickedChips = $derived(
    mentionChipsOf(
      picker.picked,
      memberById,
      archeionSession.participantId,
      rosterLoaded,
    ),
  );

  // Keys the picker owns come first (it decides from the draft and the caret);
  // the rest of this hook is the backspace-on-empty-draft gesture, which drops
  // the last pick and can never eat a character the user typed.
  function onComposeKey(event: KeyboardEvent): boolean {
    if (picker.key(event)) {
      // Arrows would move the caret, Enter would submit, Tab would leave the
      // field: none of that is wanted while the popup has the key.
      event.preventDefault();
      return true;
    }
    if (
      event.key === "Backspace" &&
      !event.isComposing &&
      composer.draft.length === 0 &&
      picker.picked.length > 0
    ) {
      picker.removeLast();
      return true;
    }
    return false;
  }

  // The row the armed quote points at, re-resolved every render against the
  // window: a row that scrolls out of it drops to the neutral author label
  // instead of showing stale copy.
  const quotedLine = $derived(
    armedSeq === undefined
      ? undefined
      : conv?.lines.find((line) => line.seq === armedSeq),
  );

  // The message wire carries the sender's kind + server-resolved stable handle
  // (the lesche derives it fresh at read time via the same resolver the roster
  // uses, so it is correct, not a send-time snapshot). <SenderIdentity>
  // decomposes it at render: `@<username>` for a human,
  // `<id-prefix>@<owner-username>` for an agent. The id-prefix is the
  // unforgeable anchor and the @owner the endorsement, so a mimic cannot pass
  // as a trusted agent. The mutable display name (label) is resolved separately
  // from the roster -- the wire `Participant` carries no label. A Cpu/User icon
  // marks agent/human authorship. Every bubble -- including the caller's own --
  // shows the same display-name + @handle (+ id for agents) header; own bubbles
  // are
  // additionally marked by right-alignment + filled-primary fill (no "you").
</script>

<svelte:head><title>{room_title()}</title></svelte:head>

<svelte:window
  onkeydown={(e) => showMembers && e.key === "Escape" && (showMembers = false)}
/>

<div class="flex flex-col h-full">
  <PageHeader>
    {#snippet title()}
      <p class="text-sm font-semibold truncate">{roomLabel}</p>
      <p class="text-xs opacity-50 truncate">
        <span class="opacity-70">{room_id_label()}</span><span class="font-mono"
          >{roomId}</span
        >
      </p>
    {/snippet}
    {#snippet badges()}
      {#if room?.visibility === "public"}
        <span class="text-xs preset-tonal-surface px-2 py-0.5 rounded-base"
          >{room_public_badge()}</span
        >
      {/if}
      {#if conv?.roster}
        <span class="text-xs opacity-60">
          {conv.roster.members.length === 1
            ? room_member_one({ count: conv.roster.members.length })
            : room_member_other({ count: conv.roster.members.length })}
        </span>
        {#if conv.roster.is_creator}
          <span class="text-xs preset-tonal-surface px-2 py-0.5 rounded-base"
            >{room_creator_badge()}</span
          >
        {/if}
      {/if}
    {/snippet}
    {#snippet actions()}
      <button
        type="button"
        class="size-10 grid place-items-center rounded-base shrink-0 disabled:opacity-60 {showMembers
          ? 'preset-filled-surface-500'
          : 'preset-tonal-surface hover:preset-filled-surface-500'}"
        aria-label={room_toggle_members_aria()}
        aria-pressed={showMembers}
        disabled={!conv || conv.status === "loading"}
        onclick={() => (showMembers = !showMembers)}
      >
        <Users class="size-4" />
      </button>
      <button
        type="button"
        class="size-10 {TONAL_ICON_SURF} shrink-0"
        aria-label={room_settings_aria()}
        onclick={() => navigate(`/rooms/${roomId}/settings`)}
      >
        <Settings class="size-4" />
      </button>
    {/snippet}
  </PageHeader>

  {#if !conv || conv.status === "loading"}
    <div class="flex-1 grid place-items-center p-6">
      <p class="text-sm opacity-60">{room_loading_history()}</p>
    </div>
  {:else}
    {#snippet threadPanel()}
      <ThreadPanel
        root={openThreadRoot ?? 0}
        lines={threadLines}
        roomLines={conv.lines}
        {memberById}
        selfId={archeionSession.participantId}
        {rosterLoaded}
        loading={threadLoading}
        failed={threadUnreadable}
        hasMore={threadHasMore}
        pages={threadPages}
        more={threadMore}
        walking={threadWalking}
        {membership}
        onClose={closeThread}
        onReply={sendReply}
        onRetry={retryThread}
        onRetryMore={retryMore}
        onLoadMore={loadMoreThread}
        onContinueWalk={continueWalk}
        onReturnToNewest={returnToNewest}
        onDiscard={discardReplyLine}
        trackRow={threadReveal.trackRow}
        highlightId={threadReveal.highlightId}
        onQuoteJump={(seq) => threadReveal.jump(seq)}
      />
    {/snippet}
    <div class="flex-1 min-h-0 flex">
      <div class="flex-1 min-h-0 flex flex-col min-w-0">
        <div class="flex-1 min-h-0 flex relative">
          <!-- A jump moves focus without a word, so the row it landed on is
           spoken here. Two slots, written in turn: two jumps that land on rows
           named the same are two separate events, and re-writing one node with
           the same text says nothing at all. -->
          <div class="sr-only" aria-live="polite" aria-atomic="true">
            {reveal.slot === 0 ? (reveal.announcement ?? "") : ""}
          </div>
          <div class="sr-only" aria-live="polite" aria-atomic="true">
            {reveal.slot === 1 ? (reveal.announcement ?? "") : ""}
          </div>
          <!-- UA scroll anchoring is retired here: the controller owns the
           anchoring (stick/observe), and an anchor adjustment racing the
           flex reflow could ship a scroll event with a large
           distance-from-bottom, detaching follow on a mere keystroke. -->
          <!-- The scroll box is the last place a discard's landing can put the
           focus, when the room empties while the composer cannot take it, so
           it is a focus target of its own, outside the tab order, with a name
           of its own: a click on the blank space below the last line focuses
           it, which is the price of the fallback having somewhere audible to
           land. -->
          <div
            class="flex-1 min-h-0 overflow-auto [overflow-anchor:none]"
            bind:this={scroll.viewport}
            tabindex="-1"
            role="group"
            aria-label={chat_transcript_aria()}
            onscroll={scroll.onScroll}
          >
            <div class="mx-auto w-full max-w-[80rem] p-4 flex flex-col gap-3">
              {#if conv.lines.length === 0}
                <p class="text-sm opacity-60 text-center mt-8">
                  {room_empty()}
                </p>
              {/if}
              {#each groups as group (group.root)}
                {@const line = group.head}
                <!-- Still-sending = own optimistic line (see roomLinePending); an
            inbound frame carries no seq yet but is delivered. -->
                {@const pending = roomLinePending(line)}
                {@const quotedSeq = line.quote}
                {@const q = quotes.get(line.seq)}
                {@const quoteTarget =
                  q?.jumpable === true ? quotedSeq : undefined}
                {@const rowLabel = roomMessageLabel(line, {
                  time: formatStamp(line.createdAt, ROW_TIME),
                  authorLabel: memberById.get(line.senderId)?.label,
                })}
                {@const chips = mentionChipsOf(
                  line.mentions,
                  memberById,
                  archeionSession.participantId,
                  rosterLoaded,
                )}
                {@const href = profileHref(
                  line.senderKind,
                  line.senderHandle,
                  line.senderTagmaId,
                )}
                {#snippet extraActions()}
                  {#if quotable(line)}
                    <QuoteButton
                      onclick={() =>
                        (pendingQuote = { scope: roomId, seq: line.seq })}
                    />
                  {/if}
                {/snippet}
                <div
                  use:reveal.trackRow={line.seq}
                  tabindex="-1"
                  class="group flex flex-col {line.mine
                    ? 'items-end'
                    : 'items-start'} {revealMark(reveal.highlightId, line.seq)}"
                >
                  <span id={roomRowAnchorId(line.seq)} class="sr-only">
                    {rowLabel}
                  </span>
                  <span
                    class="text-xs px-1 mb-0.5 max-w-[80%] min-w-0 flex items-center"
                  >
                    <SenderIdentity
                      kind={line.senderKind}
                      handle={line.senderHandle}
                      label={memberById.get(line.senderId)?.label}
                      {href}
                    />
                  </span>
                  <MessageRow mine={line.mine}>
                    <MessageBubble
                      {extraActions}
                      text={line.text}
                      markdown={line.senderKind === "agent"}
                      mine={line.mine}
                      {pending}
                      bare={pending}
                      pin={togglePin}
                      quote={q}
                      quoteDescribedBy={quoteTarget === undefined
                        ? undefined
                        : quoteAnchor(quoteTarget)}
                      mentions={chips}
                      onQuoteClick={quoteTarget === undefined
                        ? undefined
                        : () => jumpToQuote(quoteTarget)}
                    />
                  </MessageRow>
                  {#if line.mine && pending && !line.failed}
                    <span
                      class="text-xs opacity-50 animate-pulse"
                      aria-label={room_sending_aria()}>··</span
                    >
                  {:else if line.mine && line.failed}
                    <button
                      type="button"
                      class="text-xs text-error-500 dark:text-error-400 hover:underline"
                      onclick={() => {
                        // A re-failure marks the new optimistic line failed + renders
                        // its own Retry; the rejection is already surfaced, so swallow
                        // it here to avoid an unhandled rejection.
                        void roomConversationsStore
                          .resend(roomId, line)
                          .catch(() => {});
                      }}>{common_retry()}</button
                    >
                    <DeleteFailedButton
                      onConfirm={() => discardRow(line.seq)}
                    />
                  {/if}
                  {#if group.replies.length > 0 || group.rootState !== "rooted"}
                    {@const entry = threadEntry(group, countsKnown)}
                    <button
                      type="button"
                      class="mt-1 text-xs px-2 py-1 rounded-base preset-tonal-surface hover:preset-filled-surface-500 flex items-center gap-1.5 max-w-[80%] min-w-0"
                      onclick={() => void openThread(group.root)}
                    >
                      <span class="font-medium shrink-0">
                        {#if entry.count === undefined}
                          {room_thread_view()}
                        {:else if entry.count === 1}
                          {room_thread_replies_one({ count: entry.count })}
                        {:else}
                          {room_thread_replies_other({ count: entry.count })}
                        {/if}
                      </span>
                      {#if entry.last}
                        <span class="opacity-60 truncate">
                          · {room_thread_last_from({
                            handle: entry.last.senderHandle,
                          })}
                          · {formatStamp(entry.last.createdAt, ROW_TIME)}
                        </span>
                      {/if}
                    </button>
                  {/if}
                  {#if entryRootStates.get(group.root) === "older"}
                    <span class="text-xs opacity-60 mt-0.5 px-1">
                      {room_thread_orphan_note()}
                    </span>
                  {:else if entryRootStates.get(group.root) === "pending"}
                    <span class="text-xs opacity-60 mt-0.5 px-1">
                      {room_thread_pending_note()}
                    </span>
                  {/if}
                </div>
              {/each}
              {#if conv.status === "error" && conv.error}
                <p
                  class="text-xs text-error-500 dark:text-error-400 text-center"
                >
                  {conv.error}
                </p>
              {/if}
            </div>
          </div>
          {#if !scroll.follow}
            <!-- No z-index on purpose: the members panel (z-10, a later
          sibling) must win when open. -->
            <div
              class="absolute inset-x-0 bottom-4 mx-auto flex max-w-[80rem] justify-end px-4 pointer-events-none"
            >
              <ScrollToBottomButton
                missed={scroll.missed}
                onclick={() => scroll.forceBottom()}
              />
            </div>
          {/if}
          {#if showMembers}
            <!-- Backdrop: a non-interactive click-catcher that closes the panel on
          mobile (the panel is an overlay on narrow viewports; a flex column on
          wide ones). `role="presentation"` keeps it out of the a11y tree. -->
            <div
              class="absolute inset-0 bg-black/20 lg:hidden"
              role="presentation"
              onclick={() => (showMembers = false)}
            ></div>
            <aside
              class="absolute right-0 top-0 h-full w-80 max-w-[80vw] flex flex-col bg-surface-100-900 border-l border-surface-200-800 shadow-lg lg:static lg:shadow-none z-10"
              aria-label={room_members_aria()}
            >
              <div
                class="px-3 py-2 border-b border-surface-200-800 flex items-center justify-between shrink-0"
              >
                <span class="text-sm font-semibold">{room_members()}</span>
                <button
                  type="button"
                  class="size-7 {TONAL_ICON_SURF}"
                  aria-label={room_close_members_aria()}
                  onclick={() => (showMembers = false)}
                >
                  <X class="size-4" />
                </button>
              </div>
              <div class="flex-1 min-h-0 overflow-auto p-2 flex flex-col gap-1">
                {#if !conv.roster}
                  <p class="text-sm opacity-60 px-1 py-2">{common_loading()}</p>
                {:else}
                  {#each conv.roster.members as m (m.id)}
                    <MemberRow
                      member={m}
                      selfId={archeionSession.participantId}
                      isCreator={conv.roster.is_creator &&
                        m.id === archeionSession.participantId}
                      online={conv.online.has(m.id)}
                    />
                  {/each}
                {/if}
              </div>
            </aside>
          {/if}
        </div>
        <Composer
          bind:this={composerRef}
          {composer}
          {disabled}
          pendingCount={0}
          disabledNotice={room_unavailable()}
          onSubmitted={() => scroll.forceBottom()}
          onResized={() => scroll.reanchorIfFollowing()}
          onCancelQuote={() => (pendingQuote = null)}
          {onComposeKey}
          mentionAria={{
            open: picker.open,
            activeId:
              picker.activeId === undefined
                ? undefined
                : mentionOptionId(picker.activeId),
          }}
        >
          <!-- The two strips and the popup live in the composer's own slot family:
           the quote this send answers, the members it names, and the
           candidates while an `@` token is open. -->
          {#snippet quoteBar()}
            {#if armedSeq !== undefined}
              <QuoteBar
                author={quotedLine?.senderHandle}
                preview={quotedLine === undefined
                  ? undefined
                  : previewOf(quotedLine)}
                onCancel={() => (pendingQuote = null)}
              />
            {/if}
          {/snippet}
          {#snippet mentionBar()}
            {#if pickedChips.length > 0}
              <div class="mb-1">
                <MentionChips
                  chips={pickedChips}
                  onRemove={(id) => picker.remove(id)}
                />
              </div>
            {/if}
          {/snippet}
          {#snippet mentionPicker()}
            {#if picker.open}
              <MentionPicker
                rows={picker.rows}
                activeIndex={picker.activeIndex}
                limitHit={picker.limitHit}
                onPick={(id) => picker.pick(id)}
              />
            {/if}
          {/snippet}
        </Composer>
      </div>
      {#if wide && openThreadRoot !== null}
        <aside
          class="w-96 max-w-[40vw] shrink-0 min-h-0 flex flex-col border-l border-surface-200-800 bg-surface-100-900"
          aria-label={room_thread_panel_title()}
        >
          {@render threadPanel()}
        </aside>
      {/if}
    </div>
    {#if !wide && openThreadRoot !== null}
      <Dialog
        open={true}
        onOpenChange={(event) => {
          if (!event.open) closeThread();
        }}
      >
        <Portal>
          <Dialog.Backdrop class="fixed inset-0 bg-surface-50-950/60 z-50" />
          <Dialog.Positioner class="fixed inset-0 z-50">
            <Dialog.Content
              class="h-full w-full flex flex-col bg-surface-100-900"
            >
              <Dialog.Title class="sr-only">
                {room_thread_panel_title()}
              </Dialog.Title>
              <Dialog.Description class="sr-only">
                {room_thread_sheet_description()}
              </Dialog.Description>
              {@render threadPanel()}
            </Dialog.Content>
          </Dialog.Positioner>
        </Portal>
      </Dialog>
    {/if}
  {/if}
</div>
