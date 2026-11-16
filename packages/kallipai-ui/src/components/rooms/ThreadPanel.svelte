<script lang="ts">
  // The panel around one discussion: the header (its title, why the reader is
  // here, and the close button) above the reading surface. The body is
  // `ThreadConversation`, which a discussion's own page mounts without this
  // chrome; the panel forwards what its host hands it, and owns nothing but
  // the header and the two handles a host may call.
  import { X } from "@lucide/svelte";
  import ThreadConversation from "./ThreadConversation.svelte";
  import { TONAL_ICON_SURF } from "../../lib/classes.ts";
  import type { MentionTarget, RoomLine } from "../../lib/room-message.ts";
  import { type ThreadMembership, type ThreadMore } from "../../lib/threads.ts";
  import {
    room_thread_close_aria,
    room_thread_membership_mentioned,
    room_thread_membership_replied,
    room_thread_membership_started,
    room_thread_panel_title,
  } from "../../paraglide/messages.js";

  let {
    root,
    roomId,
    lines,
    roomLines,
    memberById,
    selfId,
    rosterLoaded,
    loading,
    failed,
    hasMore,
    more,
    walking,
    pages,
    membership,
    onClose,
    onReply,
    onRetry,
    onRetryMore,
    onLoadMore,
    onContinueWalk,
    onReturnToNewest,
    onDiscard,
    onResend,
    onQuoteJump,
  }: {
    /** Which discussion this panel is reading. A change is a different
     *  discussion: the draft and the scroll belong to the one that left. */
    root: number;
    /** The room the discussion lives in: the body's draft key needs it. */
    roomId: string;
    /** The rows of the discussion this panel has read, in order. */
    lines: readonly RoomLine[];
    /** The room's loaded lines. A quote in this discussion may name one of
     *  them instead of a reply: the body resolves it there, because stating
     *  that the message is missing would be false while the room draws it. */
    roomLines: readonly RoomLine[];
    memberById: ReadonlyMap<string, MentionTarget>;
    selfId: string | null;
    rosterLoaded: boolean;
    loading: boolean;
    failed: boolean;
    /** TRUE while the window the panel holds ends on a full page, so the
     *  discussion goes on below it. */
    hasMore: boolean;
    /** What the foot of the panel says, as one value. */
    more: ThreadMore;
    /** A walk is reading down the discussion: the body follows it. */
    walking: boolean;
    /** How many pages of this discussion have landed, the first one included. */
    pages: number;
    /** Why the reader is in this discussion, or null when the panel cannot
     *  say: no reason it read, or a discussion it could not read whole. */
    membership: ThreadMembership | null;
    onClose: () => void;
    /** Send one reply into this discussion. Rejects on failure; the body keeps
     *  the text and shows the shared send-failure line. */
    onReply: (text: string, quote?: number) => Promise<void>;
    onRetry: () => Promise<void>;
    onRetryMore: () => Promise<void>;
    onLoadMore: () => Promise<unknown>;
    onContinueWalk: () => Promise<void>;
    onReturnToNewest: () => Promise<void>;
    /** Drop a failed reply. Answers whether it left. */
    onDiscard: (seq: number) => boolean;
    /** Send a failed reply again; the host that holds the store supplies it. */
    onResend?: (line: RoomLine) => void;
    /** Land on a row this panel draws, when the host wants the move. */
    onQuoteJump?: (seq: number) => void;
  } = $props();

  let body = $state<{
    jump(seq: number): boolean;
    land(seq: number): boolean;
  } | null>(null);

  const membershipCopy = $derived.by(() => {
    if (membership === "started") return room_thread_membership_started();
    if (membership === "replied") return room_thread_membership_replied();
    if (membership === "mentioned") return room_thread_membership_mentioned();
    return null;
  });

  /** Land on one of the discussion's rows, through the body that draws it. */
  export function jump(seq: number): boolean {
    return body?.jump(seq) ?? false;
  }

  /** Put the reader on one of the discussion's rows without a sentence. */
  export function land(seq: number): boolean {
    return body?.land(seq) ?? false;
  }
</script>

<header
  class="px-3 py-2 border-b border-surface-200-800 flex items-start justify-between gap-2 shrink-0"
>
  <div class="min-w-0 flex flex-col gap-0.5">
    <p class="text-sm font-semibold truncate">{room_thread_panel_title()}</p>
    {#if membershipCopy}
      <p class="text-xs opacity-60">{membershipCopy}</p>
    {/if}
  </div>
  <button
    type="button"
    class="size-7 {TONAL_ICON_SURF}"
    aria-label={room_thread_close_aria()}
    onclick={onClose}
  >
    <X class="size-4" />
  </button>
</header>

<ThreadConversation
  bind:this={body}
  {root}
  {roomId}
  {lines}
  {roomLines}
  {memberById}
  {selfId}
  {rosterLoaded}
  {loading}
  {failed}
  {hasMore}
  {more}
  {walking}
  {pages}
  {onReply}
  {onRetry}
  {onRetryMore}
  {onLoadMore}
  {onContinueWalk}
  {onReturnToNewest}
  {onDiscard}
  {onResend}
  {onQuoteJump}
/>
