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
  type SignalEvent,
  openRelayChannel,
} from "@kallipai/kallip-lesche-client";
import { SvelteMap } from "svelte/reactivity";
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
import type { NavIndicator } from "../shell.ts";
import type { ConversationLine } from "../transcript.ts";

/** Map a conversation's transport status to a sidebar dot. This is OUR channel
 *  transport (the KEX/drain lifecycle), distinct from the dashboard's peer
 *  presence. `error` (KEX/drain failure) is kept distinct from `offline` (peer
 *  went away) so the sidebar can flag "click to retry" vs "asleep". */
function channelIndicator(
  status: "opening" | "open" | "offline" | "error",
): NavIndicator {
  switch (status) {
    case "open":
      return "live";
    case "opening":
      return "pending";
    case "offline":
      return "down";
    case "error":
      return "error";
  }
}

class ChannelsStore {
  /** conversationId -> conversation state. `SvelteMap` (not `$state(new
   *  Map())`): Svelte's `$state` proxy does not wrap Map/Set, so a raw Map's
   *  in-place `.set()` would be invisible to reactivity and the sidebar would
   *  never update. SvelteMap tracks membership + iteration natively; per-field
   *  reactivity of each entry still comes from the Conversation's own runes. */
  private conversations = new SvelteMap<string, ConversationBase>();

  /** tagmaIds with an in-flight openRelay() started by ensureOpen. Guards
   *  auto-connect against duplicate/racing opens. Plain (a concurrency guard,
   *  never rendered). */
  private pendingOpens = new Set<string>();

  /** Injected read-only backfill for the cached realtime status snapshot, used
   *  to seed a freshly-opened relay conversation's `statusSnapshot` so the chat
   *  header shows at once instead of waiting for the next status push. Bound by
   *  the shell (which owns both stores) to keep this store decoupled from
   *  realtime. `null` until the shell wires it (status just waits then). */
  private statusBackfill:
    | ((tagmaId: string) => import("../tagmata.svelte.ts").TagmaStatusSummary | undefined)
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

  /** Snapshot for the sidebar: conversationId/label/indicator per open relay
   *  conversation. The local conversation is never listed here (the offline
   *  sidebar is a single Chat link). Reading `c.status` also subscribes the
   *  indicator to mid-life status changes. */
  get list(): {
    conversationId: string;
    label: string | null;
    indicator: NavIndicator;
  }[] {
    const out: {
      conversationId: string;
      label: string | null;
      indicator: NavIndicator;
    }[] = [];
    for (const c of this.conversations.values()) {
      if (c.kind !== "relay") continue;
      const r = c as RelayConversation;
      out.push({
        conversationId: c.conversationId,
        label: r.label,
        indicator: channelIndicator(c.status),
      });
    }
    return out;
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
          ({ historyId, role, text, createdAt }) => ({
            historyId,
            role: role as ConversationLine["role"],
            text,
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
      this.conversations.delete("local");
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
      this.conversations.delete("local");
    }
  }

  // --- online (relay) ---

  /** Open an E2EE channel to `tagma`. Runs the key exchange, hydrates the
   *  transcript from the local cache (instant), then asks the tagma for an
   *  incremental history batch and drains. Resolves to the conversation id. */
  async openRelay(tagma: TagmaView): Promise<string> {
    const userId = agoraSession.user?.user_id;
    if (!userId) throw new Error("not signed in");

    const info = await agoraClientOrFail().getTagma(tagma.tagma_id);
    const channel = await openRelayChannel(
      lescheClientOrFail(),
      tagma.tagma_id,
      userId,
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
        ({ historyId, role, text, createdAt }) => ({
          historyId,
          // The cache stores role as an opaque string (a UI concept); cast back
          // to the reducer's role union, which the values round-trip as.
          role: role as ConversationLine["role"],
          text,
          createdAt,
        }),
      );
      conv.transcript = { lines, status: "idle" };
      conv.maxRendered = cached[cached.length - 1]!.historyId;
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
    this.conversations.set(channel.conversationId, conv);
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
    this.conversations.delete(conversationId);
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
    this.pendingOpens.clear();
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

  /** Find the open relay conversation for a tagma, if any. The map is keyed by
   *  conversationId (server-derived); auto-connect and revoke key off tagmaId. */
  private findByTagma(tagmaId: string): RelayConversation | undefined {
    for (const c of this.conversations.values()) {
      if (c.kind === "relay" && (c as RelayConversation).tagmaId === tagmaId) {
        return c as RelayConversation;
      }
    }
    return undefined;
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
