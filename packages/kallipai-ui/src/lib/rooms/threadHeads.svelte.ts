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

import { previewOf } from "../quote.ts";
import type { RoomLine } from "../room-message.ts";

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
