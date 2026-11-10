// Asking a page source for rows: the scroll-up single-flight, the "no more
// older" flag, and the one-row probe a quote block uses.
//
// The source is the conversation leaf's own (cache-first, then transport),
// handed in as two readers so this file holds no transport knowledge. The
// probe's verdicts (row / "deleted" / "error") are decided here once, never
// by a leaf: an unreadable row could be the quoted one, so it reads as an
// error rather than as absence.

import type { HistoryEntry } from "@kallipai/kallipai-lesche-client";
import { historyEntryLine } from "../../transcript.ts";
import type { ConversationLine } from "../../transcript.ts";
import type { Transcript } from "./transcript.svelte.ts";
import { WINDOW_PAGE } from "./shared.ts";

export class Pager {
  /** True while a scroll-up page is in flight (single-flight guard + the
   *  top sentinel's busy state). */
  loadingOlder = $state(false);

  /** False once an older page came back empty — the sentinel stops
   *  arming. Stays true while unknown (conservative: an extra empty
   *  pull is cheap, a missed page is not). */
  hasMoreOlder = $state(true);

  constructor(
    private readonly transcript: Transcript,
    /** The leaf page source (cache-first, then transport). */
    private readonly loadOlderPage: (k: number) => Promise<void>,
    /** The leaf's page for one probe (see peek). */
    private readonly probePage: (id: number) => Promise<HistoryEntry[] | null>,
  ) {}

  /** Fetch one page older than the window head. Template method: the
   * shared guards, the single-flight flag, and the quiet error handling
   * live here; the leaf supplies the page source via loadOlderPage.
   *
   *  minRowId === 0 means the window has not formed yet (hydrate or
   *  catch-up still in flight, or the server truly has nothing): paging
   *  older-than-nothing is ill-defined — and racing the initial fill here
   *  once issued a recent-N pull whose always-false `more` permanently
   *  disarmed the sentinel. Wait for a window head to exist. */
  async older(k = WINDOW_PAGE): Promise<void> {
    // A frozen window pages nothing: rows would land without positions and
    // without a cache copy, so it stops asking until a reload.
    if (this.transcript.frozen) return;
    if (this.loadingOlder || !this.hasMoreOlder) return;
    if (this.transcript.minRowId <= 0) return;
    this.loadingOlder = true;
    try {
      await this.loadOlderPage(k);
    } catch {
      // Offline / dead transport: retry on the next scroll-to-top.
    } finally {
      this.loadingOlder = false;
    }
  }

  /** Look up one history row by id WITHOUT touching the loaded window.
   *  A quote block asks whether its target still exists, and a page of the
   *  target's neighbourhood must never be merged: the pager only moves
   *  older from the window floor, so a mid-history segment would leave a
   *  hole that nothing can fill.
   *
   *  The decision is made here, once, for every leaf: an id at or below the
   *  rendered head is read through the leaf's own page source, and the
   *  answer is the row itself, `"deleted"` (the page held only rows below
   *  the id, or none at all: a contiguous history without it proves it is
   *  gone -- and only a COMPLETE page proves contiguity: a window the tagma
   *  could not decode whole is a hole, so its miss proves nothing) or
   *  `"error"` (an id that is not a positive integer, an id above the head,
   *  a leaf that cannot give the page, a page with undecodable rows or one
   *  that arrived short of its own count, or a page row that maps to no
   *  line -- an unreadable row could be the quoted one, and nothing could
   *  be drawn for it). The split is never a leaf's call. */
  async peek(id: number): Promise<ConversationLine | "deleted" | "error"> {
    if (!Number.isInteger(id) || id <= 0) return "error";
    // A probe is only meaningful for a rendered row: an id above the head
    // is not something a quote can point at, and its page would land above
    // the window as a live row (the reroute only catches ids at or below
    // the head), which is the hole the probe is meant never to open.
    if (id > this.transcript.maxRowId) return "error";
    const rows = await this.probePage(id);
    if (rows === null) return "error";
    for (const row of rows) {
      const line = historyEntryLine(row);
      if (line === null) return "error";
      if (line.historyId === id) return line;
    }
    return "deleted";
  }
}
