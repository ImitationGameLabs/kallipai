// ChannelsStore: the unified per-conversation manager. Holds the offline
// "local" conversation (a single LocalConversation bound to a DirectTransport)
// and the online relay conversations (RelayConversation bound to a
// RelayTransport via openRelay), keyed by conversation id in one SvelteMap. The
// shell wires realtime's envelope + signal + presence sinks into the store; the
// store routes each by conversation/tagma id to the owning conversation.
//
// The two leaves (RelayConversation / LocalConversation) share the conversation
// reducer (../transcript.ts); see conversation.svelte.ts for the per-conversation
// state and the transport-drain contract.

import { type TagmaView } from "@kallipai/kallip-agora-client";
import {
  type Envelope,
  openRelayChannel,
  type SignalEvent,
} from "@kallipai/kallip-lesche-client";
import { SvelteMap, SvelteSet } from "svelte/reactivity";
import {
  agoraClientOrFail,
  agoraSession,
  lescheClientOrFail,
} from "./agora.svelte.ts";
import type { DirectTransport } from "./directTransport.ts";
import { RelayTransport } from "./relayTransport.ts";
import {
  ConversationBase,
  LocalConversation,
  RelayConversation,
} from "./conversation.svelte.ts";
import { clearConvCache, loadAll } from "@kallipai/kallip-lesche-client";
import { configStore } from "../config/config.svelte.ts";
import type { ConversationLine } from "../transcript.ts";

/** Per-tagma transport state, exposed for the sidebar indicator and the
 *  tagma-keyed chat page (see `ChannelsStore.getTagmaChannelState`). */
export type TagmaChannelState =
  | { kind: "absent" }
  | { kind: "pending"; conversationId?: string }
  | { kind: "open"; conversationId: string }
  | { kind: "offline"; conversationId: string }
  | { kind: "error"; conversationId: string };

class ChannelsStore {
  /** conversationId -> conversation state. `SvelteMap` (not `$state(new
   *  Map())`): Svelte's `$state` proxy does not wrap Map/Set, so a raw Map's
   *  in-place `.set()` would be invisible to reactivity and the sidebar would
   *  never update. SvelteMap tracks membership + iteration natively; per-field
   *  reactivity of each entry still comes from the Conversation's own runes. */
  private conversations = new SvelteMap<string, ConversationBase>();

  /** Reverse index `tagmaId -> conversationId` for the relay conversations in
   *  `conversations`, so `findByTagma` (the sidebar dot, signal/status
   *  delivery) is O(1) instead of a per-tagma linear scan. Maintained alongside
   *  `conversations` via `setRelayConv`/`dropConv`; local conversations never
   *  carry a tagma id and never enter this map. `SvelteMap` for the same
   *  reactivity reason as `conversations`. */
  private tagmaIndex = new SvelteMap<string, string>();

  /** tagmaIds with an in-flight openRelay() started by ensureOpen. Guards
   *  auto-connect against duplicate/racing opens. `SvelteSet` (not a plain
   *  `Set`): the sidebar reads membership via `getTagmaChannelState`, and the
   *  pending -> open transition removes the id from this set AFTER the
   *  conversation is already inserted with status "open" -- so the removal is
   *  the last mutation with no other reactive source to re-fire a `$derived`.
   *  A plain Set would leave the sidebar stuck on a spinner for an open
   *  channel; SvelteSet makes the removal observable. */
  private pendingOpens = new SvelteSet<string>();

  /** Teardown generation, bumped by `tearDownAll` (and thus by `reset`, which
   *  calls it). `openRelay` captures this before its awaits and bails if it has
   *  advanced when they resolve -- a logout or mode switch during the KEX/cache
   *  hydrate must not resurrect a conversation into the already-cleared map.
   *  Mirrors `attachLocal`'s post-await `activeMode` re-check, but covers logout
   *  too (logout tears the store down without flipping the mode). */
  private generation = 0;

  /** Injected read-only backfill for the cached realtime status snapshot, used
   *  to seed a freshly-opened relay conversation's `statusSnapshot` so the chat
   *  header shows at once instead of waiting for the next status push. Bound by
   *  the shell (which owns both stores) to keep this store decoupled from
   *  realtime. `null` until the shell wires it (status just waits then). */
  private statusBackfill:
    | ((
      tagmaId: string,
    ) => import("../tagmata.svelte.ts").TagmaStatusSummary | undefined)
    | null = null;

  /** Bind the cached-status backfill. Called once by the shell at boot. */
  setStatusBackfill(
    fn: (
      tagmaId: string,
    ) => import("../tagmata.svelte.ts").TagmaStatusSummary | undefined,
  ): void {
    this.statusBackfill = fn;
  }

  /** Offline boot/switch failure surfaced on the layout banner. Set by the
   *  shell when connectDirect throws (before a local conversation exists to
   *  carry the error); cleared by attachLocal. */
  localError: unknown = $state(null);

  /** Per-tagma transport state for the sidebar + the tagma-keyed chat page.
   *  `absent` = no conversation and no open in flight; `pending` covers both an
   *  in-flight `ensureOpen` (pendingOpens) and a conversation still in KEX
   *  (status "opening"). `conversationId` is attached to every settled
   *  non-absent kind so the tagma page can delegate to ChannelChatPage once the
   *  channel exists. Reactive when read in `$derived`/`$effect`: it reads the
   *  SvelteSet (pendingOpens), the SvelteMap (findByTagma lookup), and the
   *  conversation's `$state` status. */
  getTagmaChannelState(tagmaId: string): TagmaChannelState {
    if (this.pendingOpens.has(tagmaId)) return { kind: "pending" };
    const conv = this.findByTagma(tagmaId);
    if (!conv) return { kind: "absent" };
    // A conv is normally inserted only after status "open"; the "opening" arm
    // is defensive for any future pre-open insertion path.
    switch (conv.status) {
      case "open":
        return { kind: "open", conversationId: conv.conversationId };
      case "opening":
        return { kind: "pending", conversationId: conv.conversationId };
      case "offline":
        return { kind: "offline", conversationId: conv.conversationId };
      case "error":
        return { kind: "error", conversationId: conv.conversationId };
    }
  }

  get(id: string): ConversationBase | undefined {
    return this.conversations.get(id);
  }

  /** The offline "local" conversation, if any. */
  get local(): LocalConversation | undefined {
    const c = this.conversations.get("local");
    return c?.kind === "local" ? (c as LocalConversation) : undefined;
  }

  /** True iff the local conversation is connected (offline transport bound). */
  get localConnected(): boolean {
    return this.local?.connected ?? false;
  }

  // --- offline (local) ---

  /** Bind the offline DirectTransport as the "local" conversation. Replaces
   *  sessionStore.attach(): tears down any prior local conversation first,
   *  hydrates from the shared IndexedDB cache, then starts the drain. The store
   *  entry stays keyed `"local"` (so the gate/links/`localConnected` are
   *  untouched), but its cache lives under the tagma's conversation id (shared
   *  with the online path). `conversationId` is null for a never-enrolled tagma
   *  (no durable history) -- the cache then keys `"local"` too.
   *
   *  Async because the cache hydrate must complete BEFORE the SSE drain starts
   *  (a live frame racing ahead of the hydrate would double-render). Re-checks
   *  `activeMode` after the hydrate so a mode flip during the await closes the
   *  stray transport instead of attaching it. */
  async attachLocal(
    transport: DirectTransport,
    conversationId: string | null,
  ): Promise<void> {
    this.detachLocal();
    this.localError = null;
    const cacheConversationId = conversationId ?? "local";
    const conv = new LocalConversation(this, transport, cacheConversationId);
    this.conversations.set("local", conv);
    try {
      const cached = await loadAll(cacheConversationId);
      if (cached.length > 0) {
        const lines: ConversationLine[] = cached.map(
          ({ historyId, role, text, sender, createdAt }) => ({
            historyId,
            role: role as ConversationLine["role"],
            text,
            sender,
            createdAt,
          }),
        );
        conv.transcript = { lines, status: "idle" };
        conv.maxRendered = cached[cached.length - 1]!.historyId;
      }
    } catch {
      // IndexedDB unavailable (e.g. private mode): proceed with an empty
      // transcript; live delivery still works.
    }
    // Race guard: a flip back to online during the hydrate must not leave a
    // held tagma transport attached as "local".
    if (configStore.value?.activeMode !== "offline") {
      transport.close();
      this.dropConv("local");
      return;
    }
    void conv.run();
  }

  /** Tear down the local conversation (called on detach / before a re-attach /
   *  on leaving offline mode). */
  detachLocal(): void {
    const prior = this.local;
    if (prior) {
      prior.close();
      this.dropConv("local");
    }
  }

  // --- online (relay) ---

  /** Open an E2EE channel to `tagma`. Runs the key exchange, hydrates the
   *  transcript from the local cache (instant), then asks the tagma for an
   *  incremental history batch and drains. Resolves to the conversation id. */
  async openRelay(tagma: TagmaView): Promise<string> {
    const user = agoraSession.user;
    const userId = user?.user_id;
    if (!userId) throw new Error("not signed in");
    const userHandle = user?.display_name ?? user?.username ?? userId;
    // Capture the teardown generation before the awaits below; if a logout or
    // mode switch ran tearDownAll while we were in KEX/cache, drop the channel
    // we built instead of resurrecting it into the cleared map.
    const generation = this.generation;

    const info = await agoraClientOrFail().getTagma(tagma.tagma_id);
    const channel = await openRelayChannel(
      lescheClientOrFail(),
      tagma.tagma_id,
      userId,
      userHandle,
      info.pinned_public_key,
    );
    const transport = new RelayTransport(channel);
    const conv = new RelayConversation(
      channel.conversationId,
      this,
      transport,
      tagma.tagma_id,
      tagma.label,
    );
    // Hydrate from the per-device cache before marking open, so a refresh
    // restores the conversation instantly and we only pull a delta.
    const cached = await loadAll(channel.conversationId);
    if (cached.length > 0) {
      const lines: ConversationLine[] = cached.map(
        ({ historyId, role, text, sender, createdAt }) => ({
          historyId,
          // The cache stores role as an opaque string (a UI concept); cast back
          // to the reducer's role union, which the values round-trip as.
          role: role as ConversationLine["role"],
          text,
          sender,
          createdAt,
        }),
      );
      conv.transcript = { lines, status: "idle" };
      conv.maxRendered = cached[cached.length - 1]!.historyId;
    }
    // Race guard: a teardown (logout / mode switch) during the KEX or cache
    // awaits cleared the map; drop the channel we built instead of
    // resurrecting the conversation. The return value is unused by callers;
    // the page is navigating away (login redirect / mode-flip route) anyway.
    if (generation !== this.generation) {
      transport.close();
      return channel.conversationId;
    }
    conv.status = "open";
    // Backfill the status snapshot from the realtime store's cache (the agora
    // SSE has been receiving tagma_status since login), so the header shows at
    // once instead of waiting up to the next status push (~2s). The realtime
    // status sink keeps it fresh thereafter. The read goes through an injected
    // callback so this store stays decoupled from realtime (the shell binds
    // both directions, mirroring the envelope/signal sinks).
    if (this.statusBackfill) {
      conv.setStatusSnapshot(this.statusBackfill(tagma.tagma_id));
    }
    this.setRelayConv(channel.conversationId, conv);
    const after = conv.maxRendered > 0 ? conv.maxRendered : null;
    try {
      await channel.history({ after, limit: 50 });
    } catch {
      // Non-fatal: live delivery still works. Flip live so background
      // notifications are not silently disabled for the whole session.
      conv.live = true;
    }
    // Watchdog: if no history_batch_end lands within 10s (partial delivery,
    // lost marker, relay crash), force live. The batch_end handler in
    // RelayConversation.applyReply cancels this on the normal path.
    conv.liveWatchdog = setTimeout(() => {
      if (
        this.conversations.get(channel.conversationId) === conv &&
        !conv.live
      ) {
        conv.live = true;
      }
    }, 10_000);
    void conv.run();
    return channel.conversationId;
  }

  /** Idempotent, best-effort auto-open driven by the shell on presence
   *  transitions and at boot. Skips tagmas already open or with an open in
   *  flight. A dead conversation is torn down first WITHOUT purging its cache,
   *  so the re-KEX rehydrates the prior transcript. */
  async ensureOpen(tagma: TagmaView): Promise<void> {
    if (this.pendingOpens.has(tagma.tagma_id)) return;
    const existing = this.findByTagma(tagma.tagma_id);
    if (
      existing &&
      (existing.status === "open" || existing.status === "opening")
    ) {
      return;
    }
    if (existing) this.tearDown(existing.conversationId);
    this.pendingOpens.add(tagma.tagma_id);
    try {
      await this.openRelay(tagma);
    } catch (e) {
      console.warn(
        `[channels] auto-open failed for tagma ${tagma.tagma_id}:`,
        e instanceof Error ? e.message : e,
      );
    } finally {
      this.pendingOpens.delete(tagma.tagma_id);
    }
  }

  /** Send a prompt to a conversation. Renders the optimistic line and hands off
   *  to the conversation's send path (single-in-flight pump for relay; inline
   *  POST for local). */
  send(conversationId: string, text: string): void {
    const conv = this.conversations.get(conversationId);
    if (!conv || !conv.connected) return;
    void conv.send(text);
  }

  /** Close + drop a conversation by tagma id (revoke path: no plaintext left
   *  on a shared device). No-op if none is open for the tagma. */
  closeByTagma(tagmaId: string): void {
    const conv = this.findByTagma(tagmaId);
    if (conv) this.close(conv.conversationId);
  }

  /** Close + drop a conversation and purge its IndexedDB cache. */
  close(conversationId: string): void {
    this.tearDown(conversationId);
    void clearConvCache(conversationId);
  }

  /** Detach a conversation's transport + drop its entry WITHOUT purging its
   *  cache (used by close, then the cache is purged; and by ensureOpen on
   *  reconnect, so the re-KEX rehydrates). */
  private tearDown(conversationId: string): void {
    const conv = this.conversations.get(conversationId);
    if (!conv) return;
    if (conv instanceof RelayConversation && conv.liveWatchdog) {
      clearTimeout(conv.liveWatchdog);
    }
    conv.close();
    this.dropConv(conversationId);
  }

  /** Tear down every conversation (relay + local): close transports + drop
   *  entries, PRESERVING the IndexedDB cache. Used by mode switches, so the
   *  other mode rehydrates instantly from the shared cache on re-attach. */
  tearDownAll(): void {
    for (const conv of this.conversations.values()) {
      if (conv instanceof RelayConversation && conv.liveWatchdog) {
        clearTimeout(conv.liveWatchdog);
      }
      conv.close();
    }
    this.conversations.clear();
    this.tagmaIndex.clear();
    this.pendingOpens.clear();
    // Advance the teardown generation so any in-flight openRelay whose awaits
    // straddle this clear bails instead of resurrecting its conversation.
    this.generation++;
  }

  /** Tear down every conversation AND purge its IndexedDB cache. Used on logout
   *  (so no plaintext remains on a shared device) and explicit close. Purges by
   *  each conversation's `cacheConversationId` -- NOT its store key -- because
   *  the local entry's store key is `"local"` but its cache lives under the
   *  tagma's conversation id; iterating store keys would leak the local cache. */
  reset(): void {
    const entries = Array.from(this.conversations.values());
    this.tearDownAll();
    for (const conv of entries) {
      void clearConvCache(conv.cacheConversationId);
    }
  }

  /** Find the open relay conversation for a tagma, if any. O(1) via
   *  `tagmaIndex`; the map itself is keyed by conversationId (server-derived),
   *  while auto-connect/revoke key off tagmaId. */
  private findByTagma(tagmaId: string): RelayConversation | undefined {
    const id = this.tagmaIndex.get(tagmaId);
    if (id === undefined) return undefined;
    const conv = this.conversations.get(id);
    return conv instanceof RelayConversation ? conv : undefined;
  }

  /** Insert a relay conversation under `id` and index it by its tagma id so
   *  `findByTagma` is O(1). Local conversations never reach here. */
  private setRelayConv(id: string, conv: RelayConversation): void {
    this.conversations.set(id, conv);
    this.tagmaIndex.set(conv.tagmaId, id);
  }

  /** Drop a conversation by id, removing its tagma-id index entry when it is a
   *  relay conversation whose index slot still points at this id. The slot check
   *  guards a reopen race: a reopen may already have rebound the tagma to a new
   *  conversation id, in which case the newer entry wins and is left intact. */
  private dropConv(id: string): void {
    const conv = this.conversations.get(id);
    if (
      conv instanceof RelayConversation &&
      this.tagmaIndex.get(conv.tagmaId) === id
    ) {
      this.tagmaIndex.delete(conv.tagmaId);
    }
    this.conversations.delete(id);
  }

  /** Route an inbound envelope (from realtime's SSE demux) to the conversation
   *  that owns it, by pushing it onto the underlying RelayChannel's inbound
   *  queue. Unknown ids are dropped. */
  deliver(envelope: Envelope): void {
    const conv = this.conversations.get(envelope.conversation_id);
    if (conv?.kind !== "relay") return;
    (conv as RelayConversation).relayTransport.relayChannel.enqueue(envelope);
  }

  /** Route an inbound runtime signal (from realtime's tagma_signal demux) to the
   *  owning conversation's transport signal queue; its signal drain reduces it
   *  via applySignal. Unknown tagma ids are dropped. */
  deliverSignal(tagmaId: string, signal: SignalEvent): void {
    this.findByTagma(tagmaId)?.relayTransport.enqueueSignal(signal);
  }

  /** Route an aggregate status snapshot (from realtime's tagma_status demux) to
   *  the owning relay conversation's `statusSnapshot`, so the chat header has a
   *  uniform status source (the direct path drains its own SSE status). `nil`
   *  snapshot evicts (tagma went offline). Unknown tagma ids are dropped. */
  deliverStatus(
    tagmaId: string,
    snapshot: import("../tagmata.svelte.ts").TagmaStatusSummary | undefined,
  ): void {
    this.findByTagma(tagmaId)?.setStatusSnapshot(snapshot);
  }
}

export const channelsStore = new ChannelsStore();
