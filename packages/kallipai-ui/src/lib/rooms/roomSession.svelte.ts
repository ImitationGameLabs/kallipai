// RoomSession: the room-level state a surface takes once, and gives back.
//
// Two surfaces read one room: the room transcript, and a discussion's own page.
// Both need the same registry entry, roster, per-member map and unread
// bookkeeping, and both must take them the same way -- a discussion page opened
// from a link would otherwise draw every sender as a bare id and claim no
// membership. The room page kept all of it inline; the second surface would
// have copied it, so it lives here.
//
// Lifecycle belongs to the surface, not to this module: nothing else in the
// package's rune modules owns an effect (`createAutoScroll`, `createReveal`
// both leave that to their component), so a page calls `sync()` from its own
// effect and `stop()` when it tears down.
//
// The unread bookkeeping is leased per room rather than owned per surface.
// Room -> discussion swaps two instances inside one route pattern, and the
// destroy and mount order between them is not ours to choose. With an owner
// each, the leaving surface's `leaveRoom` can land after the joining one's
// `enter`, which drops the room out of "viewing" and starts counting a badge
// for a room the reader is looking at. The two-instance interleave behind the
// 2026-09-01 live log was this shape (the room page's open effect records it).
// A lease makes the order irrelevant: the first surface in enters, the last
// one out leaves.

import { untrack } from "svelte";
import { roomLabelOf } from "./roomLabel.ts";
import { roomConversationsStore } from "../session/roomConversations.svelte.ts";
import { roomsStore } from "../session/rooms.svelte";
import { roomKey, unreadStore } from "../session/unread.svelte.ts";
import { startVisibleInterval } from "../visibleInterval.ts";

/** How long the room's slow poll waits between refreshes. */
const POLL_MS = 10_000;

/** How many surfaces currently hold each room's unread bookkeeping. */
const holders = new Map<string, number>();

/** Take the room's lease; true when this surface is the first holder. */
function hold(key: string): boolean {
  const next = (holders.get(key) ?? 0) + 1;
  holders.set(key, next);
  return next === 1;
}

/** Give the room's lease back; true when this surface was the last holder. */
function release(key: string): boolean {
  const next = (holders.get(key) ?? 0) - 1;
  if (next > 0) {
    holders.set(key, next);
    return false;
  }
  holders.delete(key);
  return true;
}

export interface RoomSessionOptions {
  /**
   * Whether leaving may report the room read up to its live cursor.
   *
   * The room page says yes: it drew the room. A discussion page says no -- it
   * drew one discussion's rows, and claiming the room's newest line would say
   * the reader saw rows nobody showed them. The badge stays up a little
   * longer, and a badge is cheaper than a false "read".
   */
  advanceOnLeave?: boolean;
}

/**
 * The room state one surface reads, plus the two commands that take it and
 * give it back. Call it at component init (it builds `$derived` cells), call
 * `sync()` from a page effect, and `stop()` on teardown.
 */
export function createRoomSession(
  roomId: () => string,
  options: RoomSessionOptions = {},
) {
  const advanceOnLeave = options.advanceOnLeave ?? true;

  const conv = $derived(roomConversationsStore.get(roomId()));
  const room = $derived(
    roomsStore.rooms.find((r) => r.room_id === roomId()) ?? null,
  );
  // Named the way the shell trail names it: one helper, so the header and
  // the breadcrumb cannot disagree about a room still resolving.
  const roomLabel = $derived(roomLabelOf(roomsStore.rooms, roomId()));
  // The roster keyed by participant id: one pass serves both the sender label
  // a bubble header draws and the members a line mentions.
  const memberById = $derived(
    new Map((conv?.roster?.members ?? []).map((m) => [m.id, m] as const)),
  );
  const rosterLoaded = $derived(conv?.roster != null);

  /** The room this surface holds, if any. */
  let held: string | null = null;
  let stopPoll: (() => void) | null = null;

  /**
   * Give the room's lease back. A surface that drew the room reports how far
   * the reader got even while another surface still holds it: a room-to-
   * discussion hop takes the joining lease before the leaving teardown
   * runs, and the destroy order inside one route pattern is not ours to
   * choose. So the viewing claim ends with the last holder, while a holder
   * that drew the room notes its cursor: `noteViewedLines` keeps the claim
   * with the surface still on screen and queues the write. Only a surface
   * whose own semantics let it advance passes a cursor on at all.
   */
  function drop(key: string): void {
    const id = key.slice("room:".length);
    const last = release(key);
    // Read the last seq off the room being left, not off the room the
    // surface moved to: a room-id change drops the old lease after
    // `roomId()` already answers with the new id.
    const lastSeq = roomConversationsStore.get(id)?.lastSeq ?? null;
    if (!last) {
      if (advanceOnLeave) unreadStore.noteViewedLines(id, lastSeq);
      return;
    }
    if (advanceOnLeave) unreadStore.leaveRoom(id, lastSeq);
    else unreadStore.leaveRoom(id);
  }

  /**
   * Take (or keep) the room, load its history, and start the slow poll. Safe
   * to call on every effect run: the second call for the same room only
   * re-issues the idempotent store commands.
   *
   * The getter is read outside the untrack, even though every command that
   * follows is untracked: this one read is how the surface learns its room
   * changed. Inside the untrack a page instance would never hear the change
   * (the route keys on the pattern, not the id, so the instance survives it),
   * and the room now on screen would stay unopened with its old lease held.
   */
  function sync(): void {
    // The getter read stays tracked: a surface whose room changed must sync
    // the room now on screen, and this call is where its effect learns that.
    const id = roomId();
    // Commands keyed on the participant id, not derivations: enter() reads the
    // fields it resets and the leave path advances them, so a tracked call
    // would re-fire its own effect.
    untrack(() => {
      const key = roomKey(id);
      if (held !== key) {
        const before = held;
        held = key;
        if (hold(key)) unreadStore.enter(key);
        if (before !== null) drop(before);
      }
      void roomConversationsStore.open(id);
      void roomConversationsStore.refreshRoster(id);
      if (stopPoll === null) {
        // The dropped-frame backstop for the room_membership_changed roster
        // nudge and for offline members catching up. A terminal error stops
        // it: the room is gone to this reader and refreshing would only loop
        // the failure.
        stopPoll = startVisibleInterval(() => {
          if (conv?.status === "error") return;
          void roomConversationsStore.refresh(roomId());
          void roomConversationsStore.refreshRoster(roomId());
        }, POLL_MS);
      }
    });
  }

  /** Give the room back: stop the poll, release the lease, end viewing. */
  function stop(): void {
    stopPoll?.();
    stopPoll = null;
    const key = held;
    held = null;
    if (key !== null) untrack(() => drop(key));
  }

  // Getters, not values: a shorthand property would be read once, when this
  // factory runs, and freeze at the empty store of that moment. The surface
  // reads these where it draws.
  return {
    get conv() {
      return conv;
    },
    get room() {
      return room;
    },
    get roomLabel() {
      return roomLabel;
    },
    get memberById() {
      return memberById;
    },
    get rosterLoaded() {
      return rosterLoaded;
    },
    sync,
    stop,
  };
}
