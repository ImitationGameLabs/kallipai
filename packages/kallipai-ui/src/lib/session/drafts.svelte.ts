// The reactive glue between a composer and the drafts store (drafts.ts).
// `bindDraft` keeps `composer.draft` and the store entry under `key()` in
// sync for the lifetime of the calling component: on mount and on every key
// change it restores the stored draft into the composer, and every composer
// draft change is persisted back. This is what makes a draft survive leaving
// and re-entering a conversation (the composer instance is per-mount and its
// $state dies with the component).
//
// Ordering on a key change is the subtle part: the restore effect must run
// BEFORE the persist effect sees the new key, or the old draft would be
// written under the new key (leaking conversation A's text into B). Two
// mechanisms combine to guarantee that: restore is a `$effect.pre` (runs
// before regular effects in the same flush) and it rewrites the draft the
// persist effect reads, so persist's re-run (queued by the key read) sees
// the restored value, not the stale one -- the persist write happens after
// restore in the same flush, with the new draft already in place.

import type { ComposerModel } from "../composer.svelte.ts";
import { chatDraftsStore } from "./drafts.ts";
import { untrack } from "svelte";

/** Bind `composer`'s draft to the store entry under `key()`. Call once from
 * a component's script body; the effects live and die with that component.
 * `key()` is re-evaluated reactively, so a conversation resolving (or the
 * page switching conversations) re-binds to the new key. */
export function bindDraft(composer: ComposerModel, key: () => string): void {
  $effect.pre(() => {
    const k = key();
    untrack(() => {
      composer.draft = chatDraftsStore.get(k);
    });
  });
  $effect(() => {
    const text = composer.draft;
    untrack(() => {
      chatDraftsStore.set(key(), text);
    });
  });
}
