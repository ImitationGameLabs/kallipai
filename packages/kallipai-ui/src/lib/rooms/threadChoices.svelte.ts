// The reader's standing in the threads of the rooms this session has drawn.
//
// One cache answers two surfaces that ask the same question: the follow control
// (the thread open in a panel or on its own page) and the room page's preview
// (every thread its window draws). Read per surface, the second ask would be
// the same request again. The key is the room and the thread's head; the value
// is what the subscription route answered.
//
// A read nobody answered is remembered as unknown rather than retried at once:
// the preview draws one row per thread in the window, and a failing route would
// otherwise be asked again on every redraw. It is not remembered forever
// either: after `UNKNOWN_RETRY_MS` the next draw asks once more, because one
// blip should not cost a thread its mark and its control for the rest of the
// session. `forgetThreadChoice` clears one key, and a write always replaces
// whatever the read left.
//
// The map is bounded (a session visits many threads, and a standing matters
// while its room is still being drawn): the oldest leaves first.

import {
  clearThreadSubscription,
  fetchThreadSubscription,
  setThreadSubscription,
  type ThreadChoice,
  type ThreadSubscriptionView,
} from "@kallipai/kallipai-lesche-client";
import { runClientEffect } from "@kallipai/kallipai-common";
import { lescheBaseUrlOrFail } from "../session/archeion.svelte.ts";

/** How many standings stay remembered, oldest out first. */
export const THREAD_CHOICES_MAX = 500;

/** How long a failed read stands before the next draw may ask again. */
export const UNKNOWN_RETRY_MS = 30_000;

/** What one key holds: the standing, or a read nobody answered -- with the
 *  moment it failed, which is where the retry window is measured from. */
type Entry =
  | { readonly view: ThreadSubscriptionView }
  | {
      readonly unknown: true;
      readonly failedAt: number;
    };

let entries = $state<Record<string, Entry>>({});

/** Reads in flight, so two draws of one thread share a request. */
const inFlight = new Map<string, Promise<ThreadSubscriptionView | null>>();

function keyOf(roomId: string, root: number): string {
  return `${roomId}:${root}`;
}

function remember(key: string, entry: Entry): void {
  const next: Record<string, Entry> = { ...entries, [key]: entry };
  const keys = Object.keys(next);
  const drop = keys.length - THREAD_CHOICES_MAX;
  for (const stale of keys.slice(0, Math.max(0, drop))) delete next[stale];
  entries = next;
}

/** The standing a read answered, or null when nothing is known about the
 *  thread (never read, or read and unreadable). */
export function threadChoiceOf(
  roomId: string,
  root: number,
): ThreadSubscriptionView | null {
  const entry = entries[keyOf(roomId, root)];
  return entry !== undefined && "view" in entry ? entry.view : null;
}

/** Read one thread's standing once: a remembered answer, the request already
 *  in flight, or a single read. A failed read answers null and is remembered as
 *  unknown; `clock` is the one time source, read at the window check and again
 *  when the failure lands, so the window runs from the answer rather than from
 *  the request. A caller that passes nothing takes the wall clock, and a test
 *  passes its own. */
export function ensureThreadChoice(
  roomId: string,
  root: number,
  clock: () => number = Date.now,
): Promise<ThreadSubscriptionView | null> {
  const key = keyOf(roomId, root);
  const entry = entries[key];
  if (entry !== undefined) {
    if ("view" in entry) return Promise.resolve(entry.view);
    if (clock() - entry.failedAt < UNKNOWN_RETRY_MS) {
      return Promise.resolve(null);
    }
  }
  const pending = inFlight.get(key);
  if (pending !== undefined) return pending;
  const read = runClientEffect(
    fetchThreadSubscription(lescheBaseUrlOrFail(), roomId, root),
  )
    .then((view) => {
      remember(key, { view });
      return view;
    })
    .catch(() => {
      remember(key, { unknown: true, failedAt: clock() });
      return null;
    })
    .finally(() => {
      inFlight.delete(key);
    });
  inFlight.set(key, read);
  return read;
}

/** Record a choice; the route's answer replaces the remembered standing. */
export async function setThreadChoice(
  roomId: string,
  root: number,
  state: ThreadChoice,
): Promise<ThreadSubscriptionView> {
  const view = await runClientEffect(
    setThreadSubscription(lescheBaseUrlOrFail(), roomId, root, state),
  );
  remember(keyOf(roomId, root), { view });
  return view;
}

/** Drop a choice and read back what the thread's default is for this reader:
 *  the default depends on who is asking (a human follows; an agent holding no
 *  row is woken only while the thread holds it as a participant), so the
 *  route's answer is the only truthful one. */
export async function clearThreadChoice(
  roomId: string,
  root: number,
): Promise<ThreadSubscriptionView | null> {
  await runClientEffect(
    clearThreadSubscription(lescheBaseUrlOrFail(), roomId, root),
  );
  forgetThreadChoice(roomId, root);
  return ensureThreadChoice(roomId, root);
}

/** Forget one thread's standing, so the next read asks again. */
export function forgetThreadChoice(roomId: string, root: number): void {
  const key = keyOf(roomId, root);
  if (entries[key] === undefined) return;
  const next: Record<string, Entry> = { ...entries };
  delete next[key];
  entries = next;
}
