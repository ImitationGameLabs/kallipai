// Headless quoting state for a chat surface: the row armed for quoting, the
// answers for targets the loaded window does not hold, and the one read-only
// probe each unresolved id gets. Same shape as composer.svelte.ts and
// transcript.svelte.ts -- the view forwards and reads, the state machine lives
// here, out of the channel transcript's render body.
//
// Three facts shape the probe:
//   - a relay content frame carries no request id (the tagma pump stamps rows,
//     it does not answer a request), so two relay probes in flight cannot be
//     told apart by their replies -- the relay leaf serialises its before-pages
//     for that reason. A direct probe is one request/response pair of its own.
//     Here one id is probed at a time and a settled answer is never re-asked.
//   - a probed row previews but does not jump: the pager only moves older from
//     the window floor, so paging a mid-history row in would render a hole
//     nothing can fill.
//   - a probe that fails is retryable, never proof that the target is gone.

import {
  findQuotedLine,
  quoteViewOfLine,
  unresolvedQuote,
  type QuoteView,
} from "./quote.ts";
import type { ConversationBase } from "./session/conversation.svelte.ts";
import type { ConversationLine } from "./transcript.ts";

/** What a peek answered, or that it is still in flight. */
type Peeked = "pending" | ConversationLine | "deleted" | "unavailable";

export interface QuotesOptions {
  /** The conversation to read. A getter, not a value: the page swaps
   *  conversations as routes change, so every call re-reads it. */
  readonly conversation: () => ConversationBase | undefined;
}

export interface Quotes {
  /** The row id armed for quoting, or null. */
  readonly quoted: number | null;
  /** What the composer's quote bar draws for the armed id: the author and
   *  preview the line had when it was armed, so the strip does not flicker as
   *  the window slides. Undefined once the page has swapped conversations. */
  readonly quotedView: QuoteView | undefined;
  /** Arm a line, replacing whatever was armed. */
  quote(line: ConversationLine): void;
  /** Disarm. */
  cancel(): void;
  /** What a row's quote block draws for `id`. Undefined means the peek has not
   *  answered yet: the block renders nothing rather than guessing. */
  resolve(id: number): QuoteView | undefined;
  /** Ask again after a failed peek (the block's retry). */
  retry(id: number): void;
  /** Ask once for every unresolved target in the loaded window. Idempotent;
   *  call it from a content effect. */
  sync(): void;
}

export function createQuotes(options: QuotesOptions): Quotes {
  let quoted = $state<number | null>(null);
  // The bar draws the line it was armed from, not whatever the window holds
  // later: a quote outlives its row scrolling out of the loaded window.
  let armedView = $state<QuoteView | null>(null);
  // Answers by quoted id. In-flight probes live in the same table as
  // "pending", so the guard in sync() is one lookup and a re-run of the
  // calling effect cannot duplicate a request.
  let peeked = $state<Record<number, Peeked>>({});

  // A row id resolves only against the conversation it came from, so the state
  // above is bound to one: a swap drops the armed id and every peeked answer.
  let owner: ConversationBase | undefined;

  function loaded(): readonly ConversationLine[] {
    return options.conversation()?.transcript.lines ?? [];
  }

  /** Read the live conversation, dropping state that belonged to another
   *  one. Calls land from effects and handlers, never from a render: they
   *  may write state.
   */
  function own(): ConversationBase | undefined {
    const conv = options.conversation();
    if (conv === owner) return conv;
    owner = conv;
    quoted = null;
    armedView = null;
    peeked = {};
    return conv;
  }

  function resolve(id: number): QuoteView | undefined {
    const held = findQuotedLine(loaded(), id);
    if (held) return quoteViewOfLine(held);
    const known = peeked[id];
    if (known === undefined || known === "pending") return undefined;
    if (typeof known === "string") return unresolvedQuote(known);
    return quoteViewOfLine(known, { jumpable: false });
  }

  async function peek(id: number): Promise<void> {
    const conv = own();
    if (!conv || peeked[id] !== undefined) return;
    peeked = { ...peeked, [id]: "pending" };
    try {
      const row = await conv.peekRow(id);
      // The reader can move to another conversation while the read is in
      // flight; own() would have cleared the table for the new one, so this
      // answer would land in state it says nothing about. Drop it.
      if (options.conversation() !== conv) return;
      peeked = { ...peeked, [id]: row === "error" ? "unavailable" : row };
    } catch {
      // A throw is a failed read too: the block offers a retry rather than
      // claiming the target is gone.
      if (options.conversation() !== conv) return;
      peeked = { ...peeked, [id]: "unavailable" };
    }
  }

  return {
    get quoted() {
      return quoted;
    },
    get quotedView() {
      // The bar draws the arming snapshot; the check below only reads, so a
      // swapped page cannot show the previous conversation's line, not even
      // for one frame before the next call clears the arming.
      if (owner !== options.conversation()) return undefined;
      return armedView ?? undefined;
    },
    quote(line) {
      own();
      quoted = line.historyId;
      armedView = quoteViewOfLine(line);
    },
    cancel() {
      quoted = null;
      armedView = null;
    },
    resolve,
    retry(id) {
      own();
      if (peeked[id] !== "unavailable") return;
      const next = { ...peeked };
      delete next[id];
      peeked = next;
      void peek(id);
    },
    sync() {
      own();
      const lines = loaded();
      for (const line of lines) {
        const id = line.quote;
        if (id === undefined) continue;
        if (findQuotedLine(lines, id)) continue;
        if (peeked[id] !== undefined) continue;
        void peek(id);
      }
    },
  };
}
