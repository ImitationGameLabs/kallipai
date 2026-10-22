// The direct-session transcript: a paged tail hydrate (25 rows per page from
// seq 0, bounded so a long transcript stays a candidate-pool tail probe) plus
// an incremental pull keyed on the last seq. Extracted from the page because
// the page instance is REUSED across route-param changes: the component that
// shows /tagma/a/direct/b is the one that later shows /tagma/a/direct/c, so an
// answer arriving late would otherwise be written into the pair the reader
// already left. Every pull therefore carries the generation it started under,
// and a pull from an older generation is dropped rather than applied --
// discarded, not re-homed: the pair it belongs to is no longer on screen and
// nothing here can stand in for a fetch it did not make.
//
// Pulls do not queue: a refresh starting while another pull is in flight
// takes over, and the older pull's answers are dropped -- a slow poll must
// not stack two copies of the same tail.
// A landed hydrate is remembered as such: an empty transcript is an answer,
// so a poll after it walks the tail instead of re-reading the window.

import type { DirectMessageRow } from "@kallipai/kallipai-client";

export type DirectPhase = "loading" | "ready" | "unavailable";

export interface DirectTranscriptOptions {
  /** One history read: rows after `afterSeq`, at most `limit`. */
  readonly fetchPage: (
    tagmaId: string,
    peerId: string,
    afterSeq: number,
    limit: number,
  ) => Promise<DirectMessageRow[]>;
  /** Rows per page (test seam; production keeps the page size). */
  readonly pageSize?: number;
  /** Hydrate bound: pages walked before the tail probe stops (test seam). */
  readonly maxPages?: number;
}

export interface DirectTranscript {
  readonly phase: DirectPhase;
  /** Rows of the pair on screen, oldest first. */
  readonly rows: DirectMessageRow[];
  /** Seq the next incremental pull starts after. */
  readonly lastSeq: number;
  /** Show `tagmaId`/`peerId`; anything still in flight for the pair that was
   *  on screen stops counting. */
  open: (tagmaId: string, peerId: string) => void;
  /** Pull once: hydrate while empty, else the tail increment. Never throws --
   *  a failure of the current pair marks it unavailable, a stale failure is
   *  dropped with its generation. */
  refresh: () => Promise<void>;
  /** Show the loading state and pull again (the page's retry affordance). */
  retry: () => Promise<void>;
  /** Stop accepting answers: the page is going away. */
  close: () => void;
}

const PAGE = 25;
const MAX_PAGES = 20;

export function createDirectTranscript(
  options: DirectTranscriptOptions,
): DirectTranscript {
  const pageSize = options.pageSize ?? PAGE;
  const maxPages = options.maxPages ?? MAX_PAGES;

  let tagmaId = "";
  let peerId = "";
  let generation = 0;
  let phase = $state<DirectPhase>("loading");
  let rows = $state<DirectMessageRow[]>([]);
  let lastSeq = $state(0);
  /** Whether a hydrate has landed for the pair on screen. */
  let loaded = false;
  /** Token of the pull allowed to write: only the newest one applies. */
  let pull = 0;

  /** True when `gen` no longer names the pair on screen. */
  function stale(gen: number): boolean {
    return gen !== generation;
  }

  async function hydrate(
    gen: number,
    tagma: string,
    peer: string,
    token: number,
  ): Promise<void> {
    let collected: DirectMessageRow[] = [];
    let after = 0;
    for (let i = 0; i < maxPages; i++) {
      const batch = await options.fetchPage(tagma, peer, after, pageSize);
      if (token !== pull || stale(gen)) return;
      collected = [...collected, ...batch];
      if (batch.length < pageSize) break;
      after = batch[batch.length - 1]!.seq;
    }
    rows = collected;
    lastSeq = collected.reduce((max, r) => Math.max(max, r.seq), 0);
    loaded = true;
    phase = "ready";
  }

  async function increment(
    gen: number,
    tagma: string,
    peer: string,
    afterSeq: number,
    token: number,
  ): Promise<void> {
    const batch = await options.fetchPage(tagma, peer, afterSeq, pageSize);
    if (token !== pull || stale(gen)) return;
    if (batch.length > 0) {
      rows = [...rows, ...batch];
      lastSeq = batch[batch.length - 1]!.seq;
    }
    loaded = true;
    phase = "ready";
  }

  async function refresh(): Promise<void> {
    if (tagmaId === "") return;
    const gen = generation;
    // A later pull supersedes an older one rather than queueing behind it.
    const token = ++pull;
    const tagma = tagmaId;
    const peer = peerId;
    try {
      if (!loaded) await hydrate(gen, tagma, peer, token);
      else await increment(gen, tagma, peer, lastSeq, token);
    } catch {
      if (token !== pull || stale(gen)) return;
      phase = "unavailable";
    }
  }

  function open(tagma: string, peer: string): void {
    generation += 1;
    tagmaId = tagma;
    peerId = peer;
    phase = "loading";
    rows = [];
    lastSeq = 0;
    loaded = false;
  }

  async function retry(): Promise<void> {
    phase = "loading";
    await refresh();
  }

  function close(): void {
    generation += 1;
  }

  return {
    get phase() {
      return phase;
    },
    get rows() {
      return rows;
    },
    get lastSeq() {
      return lastSeq;
    },
    open,
    refresh,
    retry,
    close,
  };
}
