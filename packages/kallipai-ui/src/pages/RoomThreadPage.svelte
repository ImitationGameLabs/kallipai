<script lang="ts">
  // One discussion at an address of its own: the root line and the replies
  // filed under it, at /rooms/<id>/threads/<root>. The room transcript links
  // here instead of opening a panel, so a discussion can be kept, refreshed
  // and handed to someone.
  //
  // The page composes what it shares with the room -- the room session for the
  // registry entry and the roster a sender label needs, the thread window for
  // the pages and the walk -- with the same reading surface the panel mounts.
  // It says which discussion is open and where the reader was sent inside it:
  // `?at=` names a row to land on, `?quote=` a row the composer answers, which
  // is how the room's quote button aims a reply here.
  import { untrack } from "svelte";
  import PageHeader from "../components/PageHeader.svelte";
  import ThreadConversation from "../components/rooms/ThreadConversation.svelte";
  import ThreadFollowControl from "../components/rooms/ThreadFollowControl.svelte";
  import { createRoomSession } from "../lib/rooms/roomSession.svelte.ts";
  import { createThreadWindow } from "../lib/rooms/threadWindow.svelte.ts";
  import { threadHeadOf } from "../lib/rooms/threadHeads.svelte.ts";
  import { rowParamOf } from "../lib/rooms/rowParam.ts";
  import { roomPath, roomThreadPath } from "../lib/shell/routes.ts";
  import { navigate } from "../lib/shell/port.ts";
  import { roomConversationsStore } from "../lib/session/roomConversations.svelte.ts";
  import { archeionSession } from "../lib/session/archeion.svelte.ts";
  import { threadDiscussion, threadIndex } from "../lib/threads.ts";
  import type { RoomLine } from "../lib/room-message.ts";
  import { room_thread_view } from "../paraglide/messages.js";

  let {
    roomId,
    root,
    at,
    quote,
  }: { roomId: string; root: string; at?: string; quote?: string } = $props();

  // The rows this address names. The root is in the path, so an address that
  // names no row at all is stale or hand-edited: the room is the only place
  // such a link could have meant. `at` and `quote` are optional, and their
  // absence is not an error.
  const rootSeq = $derived(rowParamOf(root));
  const atSeq = $derived(rowParamOf(at));
  const quoteSeq = $derived(rowParamOf(quote));

  // A discussion page does not report the room read: it drew one discussion's
  // rows, and claiming the room's newest line would say the reader saw rows
  // nobody showed them (see roomSession).
  const session = createRoomSession(() => roomId, { advanceOnLeave: false });
  const conv = $derived(session.conv);
  const roomLabel = $derived(session.roomLabel);
  const memberById = $derived(session.memberById);
  const rosterLoaded = $derived(session.rosterLoaded);
  // The room's loaded rows, for the discussion a quote target belongs to: a
  // head's discussion is itself, a reply is filed under another row.
  const lineIndex = $derived(threadIndex(conv?.lines ?? []));

  // The reading surface, for the moves that need its row table: a landing on
  // one of its rows and a quote of one it draws.
  let body = $state<{
    land(seq: number): boolean;
  } | null>(null);

  const threadWindow = createThreadWindow({
    roomId: () => roomId,
    root: () => rootSeq,
    lines: () => conv?.lines ?? [],
    // A walk that finds the row it was sent for puts the reader on it:
    // `?at=` and a quote aimed from the room both ride this path.
    onLanded: (seq) => body?.land(seq),
  });

  /** The discussion's own words: its opening line once read, the trail's copy
   *  of that line before this page has read anything, and the trail's label for
   *  a discussion nothing has read. */
  const title = $derived.by(() => {
    const seq = rootSeq;
    if (seq === null) return room_thread_view();
    const head = threadWindow.lines[0];
    const words =
      head !== undefined && head.seq === seq
        ? head.text.replace(/\s+/gu, " ")
        : threadHeadOf(roomId, seq);
    return words ?? room_thread_view();
  });

  // Open the room (the roster a sender label reads) and the discussion. The
  // page instance outlives a room or discussion change -- the route keys on the
  // pattern -- so both follow the getters they were given.
  $effect(() => {
    if (!archeionSession.participantId) return;
    session.sync();
    return () => untrack(() => session.stop());
  });
  // The first page waits for the participant id: `fetchThreadPage` bakes each
  // row's `mine` from it, so a page read before it draws the reader's own
  // replies as someone else's. The gate also makes the read follow the id's
  // arrival -- a same-key sync does nothing, so without it a read that failed
  // for want of an identity would never be retried.
  $effect(() => {
    if (!archeionSession.participantId) return;
    void roomId;
    void rootSeq;
    threadWindow.sync();
  });

  // An address that names no discussion goes back to its room rather than
  // drawing an empty page. `replaceState`: the bad address must not stay in the
  // history behind the reader.
  $effect(() => {
    if (rootSeq !== null) return;
    void navigate(roomPath(roomId), { replaceState: true });
  });

  // `?at=` names the row the reader was sent to, and it lands once per link.
  // The row may sit pages below the root (a quote of a reply is the reason this
  // page carries the parameter at all), so the walk that reads it is the same
  // one a quote jump takes; a row no page brings stops at what was read, with
  // the foot saying so.
  let landedKey: string | null = null;
  $effect(() => {
    const landAt = atSeq;
    const seq = rootSeq;
    const key = `${roomId}:${root}:${at ?? ""}`;
    if (landedKey === key) return;
    if (landAt === null || seq === null) return;
    if (threadWindow.loading || threadWindow.unreadable) return;
    if (threadWindow.pages === 0) return;
    landedKey = key;
    untrack(() => void threadWindow.walkAndLand(seq, landAt));
  });

  // `?quote=` names the row the composer answers. The surface arms the box once
  // it draws the row; a row on a page below the root is read first, through the
  // same walk, and a walk that comes back without it leaves the box unarmed
  // with the foot saying why.
  $effect(() => {
    const target = quoteSeq;
    const seq = rootSeq;
    if (target === null || seq === null) return;
    if (threadWindow.loading || threadWindow.unreadable) return;
    if (threadWindow.pages === 0) return;
    if (threadWindow.lines.some((line) => line.seq === target)) return;
    if (!threadWindow.hasMore || threadWindow.walking) return;
    if (threadWindow.more !== "idle") return;
    untrack(() => void threadWindow.walkTo(target));
  });

  /** The discussion a quote target belongs to: a row this window draws belongs
   *  to the discussion on screen, whatever the room's loaded window holds; a row
   *  it does not draw is read from the room's rows, where a reply names the
   *  discussion it was filed under. */
  function quoteRootOf(seq: number): number | undefined {
    if (threadWindow.lines.some((line) => line.seq === seq)) {
      return rootSeq ?? undefined;
    }
    const line = lineIndex.get(seq);
    return line === undefined ? undefined : threadDiscussion(line, lineIndex);
  }

  /** A quote block inside this discussion lands on the row it names. A quote of
   *  another discussion's reply leaves this page for that one, landing on the
   *  row: the link names both, and only the landing is left to do there. */
  function quoteJump(seq: number): void {
    const target = quoteRootOf(seq);
    if (target === undefined || target === rootSeq) {
      void body?.land(seq);
      return;
    }
    void navigate(roomThreadPath(roomId, target, { at: seq }));
  }

  /** The box stopped answering the row the link named: drop the parameter, so a
   *  refresh opens the discussion without a reply aimed at it. */
  function armSettled(): void {
    const seq = rootSeq;
    if (quoteSeq === null || seq === null) return;
    void navigate(roomThreadPath(roomId, seq), { replaceState: true });
  }

  /** Drop a failed reply: the store's reducer alone, because the surface keeps
   *  its own focus. */
  function discardReply(seq: number): boolean {
    return roomConversationsStore.discard(roomId, seq);
  }

  /** Send a failed reply again from where it failed: the store retries the same
   *  row, and a second failure marks its own line, which draws its own retry. */
  function resendReply(line: RoomLine): void {
    void roomConversationsStore.resend(roomId, line).catch(() => {});
  }

  /** Read down to the discussion's newest reply: the pill, and the way back
   *  after a reply of the reader's own. */
  async function returnToNewest(): Promise<void> {
    await threadWindow.walkTo("newest");
  }
</script>

<svelte:head><title>{title}</title></svelte:head>

<div class="flex flex-col h-full">
  <PageHeader>
    {#snippet title()}
      <p class="text-sm font-semibold truncate">{title}</p>
      <p class="text-xs opacity-50 truncate">{roomLabel}</p>
    {/snippet}
    {#snippet actions()}
      {#if rootSeq !== null}
        <ThreadFollowControl {roomId} root={rootSeq} />
      {/if}
    {/snippet}
  </PageHeader>

  {#if rootSeq !== null}
    <div class="flex-1 min-h-0 flex flex-col">
      <ThreadConversation
        bind:this={body}
        root={rootSeq}
        {roomId}
        lines={threadWindow.lines}
        roomLines={conv?.lines ?? []}
        {memberById}
        selfId={archeionSession.participantId}
        {rosterLoaded}
        loading={threadWindow.loading}
        failed={threadWindow.unreadable}
        hasMore={threadWindow.hasMore}
        more={threadWindow.more}
        walking={threadWindow.walking}
        pages={threadWindow.pages}
        onReply={threadWindow.sendReply}
        onRetry={threadWindow.retryThread}
        onRetryMore={threadWindow.retryMore}
        onLoadMore={threadWindow.loadMoreThread}
        onContinueWalk={threadWindow.continueWalk}
        onReturnToNewest={returnToNewest}
        onDiscard={discardReply}
        onResend={resendReply}
        onQuoteJump={quoteJump}
        armAt={quoteSeq ?? undefined}
        onArmSettled={armSettled}
      />
    </div>
  {/if}
</div>
