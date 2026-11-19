// Discussion heads: the words a discussion is named by before any surface has
// read it.
//
// The shell trail names a discussion by its first line, and the trail renders
// on every page, so it is drawn before a discussion's own page has read
// anything. The window records the head when the page the root sits on lands,
// and the trail reads it from here. A discussion no surface has read yet names
// itself by the trail's own label (`room_thread_view`).
//
// The map is bounded: a session visits many discussions, and a head is worth
// keeping only while a trail might draw it. The oldest leaves first, which is
// the one a reader is least likely to come back to.

import {
  fetchRoomMessages,
  type RoomMessageView,
} from "@kallipai/kallipai-lesche-client";
import { decodeB64, runClientEffect } from "@kallipai/kallipai-common";
import { previewOf } from "../quote.ts";
import { decodeRoomMessage, type RoomLine } from "../room-message.ts";
import { lescheBaseUrlOrFail } from "../session/archeion.svelte.ts";

/** How much of a discussion's first line a trail carries: enough to recognize
 *  the discussion beside its room's name, not a second line of the bar. */
export const THREAD_HEAD_LIMIT = 20;

/** How many heads stay remembered, oldest out first. */
export const THREAD_HEADS_MAX = 200;

let heads = $state<Record<string, string>>({});

/** The words a discussion is named by, once a surface has read its root page. */
export function threadHeadOf(roomId: string, root: number): string | null {
  return heads[`${roomId}:${root}`] ?? null;
}

/** Record a discussion's head. The first write wins: the root's page is read
 *  once per surface, every later page starts below it, and the root line is
 *  what the trail names anyway -- a second write could only repeat it. */
export function rememberThreadHead(
  roomId: string,
  root: number,
  head: string,
): void {
  const key = `${roomId}:${root}`;
  if (heads[key] !== undefined) return;
  const next: Record<string, string> = { ...heads, [key]: head };
  const keys = Object.keys(next);
  const drop = keys.length - THREAD_HEADS_MAX;
  for (const stale of keys.slice(0, Math.max(0, drop))) delete next[stale];
  heads = next;
}

/** Record a discussion's head from the line the root sits on: the line's own
 *  opening words, collapsed to one display line and cut short. */
export function rememberThreadHeadOf(
  roomId: string,
  root: number,
  line: RoomLine,
): void {
  rememberThreadHead(roomId, root, previewOf(line, THREAD_HEAD_LIMIT));
}

/** How long a notification waits for one head read. The shell's client allows
 *  a request 30 seconds, and a title that arrives that late arrives after the
 *  message it describes; a read that has not answered by this point is treated
 *  as unanswerable and the room's name stands. */
export const HEAD_READ_DEADLINE_MS = 1_500;

/** One row by position: the backward page whose newest row is `seq` itself.
 *
 *  The backward page is the one whose meaning ("the newest rows below this
 *  position") the route states, and it keeps answering with `seq` whatever the
 *  `?thread` page becomes once a discussion can be paged backwards. A forward
 *  position would answer the room's newest row instead. Anything but exactly
 *  the requested row -- a different position, an empty page, a failed request,
 *  a read past its deadline -- answers null. */
async function readRow(
  roomId: string,
  seq: number,
  read: (
    roomId: string,
    options: { beforeSeq?: number; limit?: number },
  ) => Promise<readonly RoomMessageView[]>,
  deadlineMs: number,
): Promise<RoomMessageView | null> {
  let rows: readonly RoomMessageView[];
  try {
    rows = await withDeadline(
      read(roomId, { beforeSeq: seq + 1, limit: 1 }),
      deadlineMs,
      [] as readonly RoomMessageView[],
    );
  } catch {
    return null;
  }
  const row = rows[0];
  return row !== undefined && row.seq === seq ? row : null;
}

/** `work`, or `fallback` once `ms` passes. The timer is cleared on either
 *  answer, so a pending request does not keep the result waiting. */
async function withDeadline<T>(
  work: Promise<T>,
  ms: number,
  fallback: T,
): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const expired = new Promise<T>((resolve) => {
    timer = setTimeout(() => resolve(fallback), ms);
  });
  try {
    return await Promise.race([work, expired]);
  } finally {
    clearTimeout(timer);
  }
}

/** The words a row carries, when it carries a message this module can read. */
function headTextOf(row: RoomMessageView): string | null {
  try {
    const decoded = decodeRoomMessage(decodeB64(row.ciphertext));
    if (decoded.op !== "message") return null;
    return previewOf(decoded, THREAD_HEAD_LIMIT);
  } catch {
    return null;
  }
}

/** The head of one discussion, read once when nothing has recorded it: the
 *  words a screen may name the thread by before its page has been opened.
 *
 *  `declared` is what the sender put in the message: it names the discussion by
 *  a position inside it, and any row of a discussion names the same one. The
 *  row it names therefore carries the head's position in its own `thread`
 *  field; that field, not the declared position, is what this resolves, and a
 *  second read fetches the head itself. The head row is the one whose `thread`
 *  is empty -- the store normalizes every reply's root to the head's position,
 *  so a head that named another row is not a shape this log writes, and one is
 *  refused rather than followed further.
 *
 *  A declaration that names the head is one read; one that names a reply is
 *  two. Each read gives up after `deadlineMs`, because the notification is
 *  already waiting and the room's name is always true. Anything else -- a
 *  failed request, a different row, an empty page, a payload that does not
 *  decode -- answers null and records nothing, and the caller names the room
 *  alone.
 */
export async function resolveThreadHead(
  roomId: string,
  declared: number,
  read: (
    roomId: string,
    options: { beforeSeq?: number; limit?: number },
  ) => Promise<readonly RoomMessageView[]> = (id, options) =>
    runClientEffect(fetchRoomMessages(lescheBaseUrlOrFail(), id, options)),
  deadlineMs: number = HEAD_READ_DEADLINE_MS,
): Promise<string | null> {
  const known = threadHeadOf(roomId, declared);
  if (known !== null) return known;

  const named = await readRow(roomId, declared, read, deadlineMs);
  if (named === null) return null;

  const root = named.thread ?? named.seq;
  const cached = threadHeadOf(roomId, root);
  if (cached !== null) return cached;

  const headRow =
    root === named.seq ? named : await readRow(roomId, root, read, deadlineMs);
  if (headRow === null || (headRow.thread ?? null) !== null) return null;

  const head = headTextOf(headRow);
  if (head === null) return null;
  rememberThreadHead(roomId, root, head);
  return head;
}
