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

import { type TagmaView } from "@kallipai/kallip-archeion-client";
import {
  type Envelope,
  type FileAttachment,
  LescheApiError,
  openRelayChannel,
  type RelayChannel,
  type SignalEvent,
} from "@kallipai/kallip-lesche-client";
import { SvelteMap, SvelteSet } from "svelte/reactivity";
import {
  archeionClientOrFail,
  archeionSession,
  lescheClientOrFail,
} from "./archeion.svelte.ts";
import { unreadStore } from "./unread.svelte.ts";
import type { DirectTransport } from "./directTransport.ts";
import { RelayTransport } from "./relayTransport.ts";
import {
  cachedLineToLine,
  ConversationBase,
  LocalConversation,
  OfflineConversation,
  RelayConversation,
  WINDOW_PAGE,
} from "./conversation.svelte.ts";
import {
  clearConvCache,
  clearPendings,
  getReadWatermark,
  readTail,
} from "@kallipai/kallip-lesche-client";
import { configStore } from "../config/config.svelte.ts";
import { type ConversationLine, LOCAL_OPERATOR_SENDER } from "../transcript.ts";

/** The last conversation id each conversation partition resolved to, so an
 * offline open can hydrate the cached transcript without a server round
 * trip (the relay conversation id is server-derived and otherwise unknown
 * offline; direct remembers under the "local" key). A derived id -- no
 * content -- so localStorage is an acceptable home; best-effort both ways. */
function convOfKey(partition: string): string {
  return "kallip-relay:conv-of:" + partition;
}

function rememberConversationOf(
  partition: string,
  conversationId: string,
): void {
  try {
    localStorage.setItem(convOfKey(partition), conversationId);
  } catch {
    // storage blocked: the offline view just starts empty
  }
}

function lastConversationOf(partition: string): string | undefined {
  try {
    return localStorage.getItem(convOfKey(partition)) ?? undefined;
  } catch {
    return undefined;
  }
}

/** Automatic channel-open backoff: first retry after 1s, doubling, capped at
 *  60s; six straight failures silence the automatic path for the session. */
const OPEN_BACKOFF_BASE_MS = 1_000;
const OPEN_BACKOFF_CAP_MS = 60_000;
const OPEN_FAILURE_LIMIT = 6;

/** Failure memory backing `ensureOpen`'s automatic path. */
interface OpenBudget {
  failures: number;
  cooldownUntil: number;
  terminal: boolean;
}
/** Per-tagma transport state, exposed for the sidebar indicator and the
 *  tagma-keyed chat page (see `ChannelsStore.getTagmaChannelState`). */
export type TagmaChannelState =
  | { kind: "absent" }
  | { kind: "unavailable" }
  | { kind: "pending"; conversationId?: string }
  | { kind: "open"; conversationId: string }
  | { kind: "offline"; conversationId: string }
  | { kind: "error"; conversationId: string };

export class ChannelsStore {
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

  /** Per-tagma auto-open failure budget (backoff cooldown + terminal
   *  flag). `SvelteMap` so the chat page can react when the automatic
   *  path gives up (`isAutoOpenExhausted`); writes happen only on open
   *  failures/success, so the reactivity cost is negligible. */
  private openBudgets = new SvelteMap<string, OpenBudget>();

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

  /** Injected stale-presence corrector: the shell (which owns both stores)
   *  binds this to realtime's markOffline, so an offline-class open failure
   *  can retract a stale-online entry without this store depending on
   *  realtime. Same decoupling reason as the status backfill above. */
  private offlineCorrection: ((tagmaId: string) => void) | null = null;

  /** Bind the stale-presence corrector. Called once by the shell at boot. */
  setOfflineCorrection(fn: (tagmaId: string) => void): void {
    this.offlineCorrection = fn;
  }

  /** Offline boot/switch failure surfaced on the layout banner. Set by the
   *  shell when connectDirect throws (before a local conversation exists to
   *  carry the error); cleared by attachLocal. */
  localError: unknown = $state(null);

  /** Per-tagma transport state for the sidebar + the tagma-keyed chat page.
   * `absent` = no conversation and no open in flight and no failure on
   * record (auto-open is about to try); `unavailable` = no conversation
   * but the failure budget holds an entry (cooldown or session-terminal:
   * nothing is in flight and nothing will retry until a presence
   * transition or an explicit retry, so the sidebar shows a settled dot
   * instead of a spinner); `pending` covers both an
   * in-flight `ensureOpen` (pendingOpens) and a conversation still in KEX
   * (status "opening"). `conversationId` is attached to every settled
   * non-absent kind so the tagma page can delegate to ChannelChatPage once the
   * channel exists. Reactive when read in `$derived`/`$effect`: it reads the
   * SvelteSet (pendingOpens), the SvelteMap (findByTagma lookup, openBudgets),
   * and the conversation's `$state` status. */
  getTagmaChannelState(tagmaId: string): TagmaChannelState {
    if (this.pendingOpens.has(tagmaId)) return { kind: "pending" };
    const conv = this.findByTagma(tagmaId);
    if (!conv) {
      // A recorded failure with no live conversation: the settled state.
      // isAutoOpenFailed and the chat page's unavailable row read the same
      // budget entry, so the sidebar and the chat page cannot disagree.
      return this.openBudgets.has(tagmaId)
        ? { kind: "unavailable" }
        : { kind: "absent" };
    }
    // A conv is normally inserted only after status "open"; the "opening" arm
    // is defensive for any future pre-open insertion path.
    switch (conv.status) {
      case "open":
        return { kind: "open", conversationId: conv.conversationId };
      case "opening":
        return { kind: "pending", conversationId: conv.conversationId };
      case "reconnecting":
        // The channel is established; only the direct SSE stream is mid-retry
        // (the transport reconnects transparently). Reporting "open" keeps
        // the sidebar indicator steady while the chat body shows its spinner.
        return { kind: "open", conversationId: conv.conversationId };
      case "offline":
        return { kind: "offline", conversationId: conv.conversationId };
      case "error":
        return { kind: "error", conversationId: conv.conversationId };
    }
  }

  /** The store key a chat page delegates to ChannelChatPage with, for a
   * tagma id: the conversation id once the channel settled, undefined
   * while absent/pending/unavailable. The shell's mobile top row reads
   * this to lift the status line -- the pathname carries the tagma id,
   * the store keys by conversation id (the same resolution the tagma
   * chat page performs before delegating). */
  conversationIdForTagma(tagmaId: string): string | undefined {
    const state = this.getTagmaChannelState(tagmaId);
    return state.kind === "open" ||
      state.kind === "offline" ||
      state.kind === "error"
      ? state.conversationId
      : undefined;
  }
  /** The open relay channel for a tagma, or null (absent / still KEXing /
   * not open). For the non-stream ops that ride the channel's control plane
   * (the direct-session store's session/history pulls via the manage
   * bridge); reach for nothing else here -- the channel lifecycle stays
   * owned by ensureOpen/close. */
  relayChannelOf(tagmaId: string): RelayChannel | null {
    const conv = this.findByTagma(tagmaId);
    return conv instanceof RelayConversation && conv.status === "open"
      ? conv.relayTransport.relayChannel
      : null;
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
   *  -- rows still persist to the operator (`NULL`) partition, but there is no
   *  tagma conversation-id, so the cache then keys `"local"` too.
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
    rememberConversationOf("local", cacheConversationId);
    const conv = new LocalConversation(this, transport, cacheConversationId);
    // Wire the transport's stream-retry lifecycle into the conversation:
    // reconnecting flips its status (spinner + composer gate), resumed
    // re-opens it and backfills the gap via catch-up.
    transport.onState = (s) => conv.onTransportState(s);
    this.conversations.set("local", conv);
    try {
      const cached = await readTail(cacheConversationId, WINDOW_PAGE);
      if (cached.length > 0) {
        const lines: ConversationLine[] = cached.map(cachedLineToLine);
        conv.transcript = { lines, status: "idle" };
        conv.minRendered = cached[0]!.historyId;
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
    // Catch the gap between the hydrated high-water mark and the newest
    // server row (a fresh device hydrates nothing and gets one recent
    // batch here). Fire-and-forget: the merge is live-safe by design.
    void conv.catchUp();
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
    const user = archeionSession.user;
    const userId = user?.user_id;
    if (!userId) throw new Error("not signed in");
    const userHandle = user?.display_name ?? user?.username ?? userId;
    // Capture the teardown generation before the awaits below; if a logout or
    // mode switch ran tearDownAll while we were in KEX/cache, drop the channel
    // we built instead of resurrecting it into the cleared map.
    const generation = this.generation;

    const info = await archeionClientOrFail().getTagma(tagma.tagma_id);
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
    // Hydrate a tail window from the per-device cache before marking open,
    // so a refresh restores the conversation instantly and catchUp pulls
    // only the delta (mirrors attachLocal; the same per-tagma cache the
    // offline entry uses, so a mode switch rehydrates from the same rows).
    try {
      const cached = await readTail(channel.conversationId, WINDOW_PAGE);
      if (cached.length > 0) {
        conv.transcript = {
          lines: cached.map(cachedLineToLine),
          status: "idle",
        };
        conv.minRendered = cached[0]!.historyId;
        conv.maxRendered = cached[cached.length - 1]!.historyId;
      }
    } catch {
      // IndexedDB unavailable (e.g. private mode): proceed empty; the
      // catch-up pull lands one recent batch.
    }

    // Rehydrate the unread watermark before the drain starts: the store's
    // per-observe persists only help if the baseline landed first, and the
    // cache read above is already openRelay's serialization point. A fresh
    // device (null) arms the store's seed-pending (first observed line
    // becomes the watermark instead of counting the backlog).
    unreadStore.hydrateTagma(
      tagma.tagma_id,
      await getReadWatermark(tagma.tagma_id),
    );
    // A successful open supersedes any degraded offline view: drop it (an
    // expired mapping could key it differently, so match by tagma).
    for (const [key, conv] of this.conversations) {
      if (
        conv instanceof OfflineConversation &&
        conv.tagmaId === tagma.tagma_id
      ) {
        this.conversations.delete(key);
      }
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
    // Backfill the status snapshot from the realtime store's cache (the archeion
    // SSE has been receiving tagma_status since login), so the header shows at
    // once instead of waiting up to the next status push (~2s). The realtime
    // status sink keeps it fresh thereafter. The read goes through an injected
    // callback so this store stays decoupled from realtime (the shell binds
    // both directions, mirroring the envelope/signal sinks).
    if (this.statusBackfill) {
      conv.setStatusSnapshot(this.statusBackfill(tagma.tagma_id));
    }
    this.setRelayConv(channel.conversationId, conv);
    // Catch up from the hydrated high-water to the newest server row
    // (a fresh device gets one recent batch). Fire-and-forget, mirroring
    // attachLocal: the drain runs concurrently, the UI is interactive
    // meanwhile, and RelayConversation.catchUp owns the pull loop +
    // per-batch timeouts + the notification floor.
    void conv.run();
    void conv.catchUp();
    // Flush the durable unsent lines (offline/failed sends) once the
    // transport is live: the relay face of the reconnect auto-retry.
    void conv.retryAllPending();
    rememberConversationOf(tagma.tagma_id, channel.conversationId);
    return channel.conversationId;
  }

  /** Idempotent, best-effort auto-open driven by the shell on presence
   *  transitions and at boot. Skips tagmas already open or with an open in
   *  flight. A dead conversation is torn down first WITHOUT purging its cache,
   *  so the re-KEX rehydrates the prior transcript.
   *
   *  `explicit` marks a user-initiated open (the chat page's mount or its
   *  retry button): it bypasses the failure budget's gates -- a failed
   *  explicit open still counts, so the session terminal silences the
   *  automatic path without ever locking the user out.
   *
   *  `refresh` (the presence sink's online transition) re-keys an ALREADY
   *  open channel: the open-status early-return is skipped so the existing
   *  conversation is torn down WITHOUT purging its cache and re-opened -- a
   *  restarted peer runs a fresh epoch that cannot read our old session
   *  key, so sends on the stale channel would be 202-then-silently-dropped.
   *  An `opening` channel still early-returns: that open settles on a fresh
   *  epoch anyway. */
  async ensureOpen(
    tagma: TagmaView,
    opts: { explicit?: boolean; refresh?: boolean } = {},
  ): Promise<void> {
    // Guard note: a refresh arriving while an openRelay is
    // mid-flight is swallowed here, and that open settles on whatever epoch
    // its KEX caught -- so a peer restart inside that millisecond window
    // can still land a stale-epoch channel, which survives until the NEXT
    // online transition re-keys it. Same shape as the issue this refresh
    // cures, not a new failure mode; accepted.
    if (this.pendingOpens.has(tagma.tagma_id)) return;
    const budget = this.openBudgets.get(tagma.tagma_id);
    if (opts.explicit) {
      // Bypass the terminal/cooldown gates (user intent outranks failure
      // history) but do NOT clear the budget: a failed explicit open
      // still counts, so an effect re-fire cannot turn into an unbounded
      // storm; only success clears it.
    } else if (budget?.terminal) {
      return;
    } else if (budget && this.now() < budget.cooldownUntil) {
      return;
    }
    const existing = this.findByTagma(tagma.tagma_id);
    // Early-return unless a refresh explicitly re-keys an open channel: an
    // in-flight `opening` always settles on a fresh epoch, but a settled
    // `open` may be riding a stale epoch after a peer restart -- the refresh
    // (presence back online) drops it through to the tearDown + re-KEX below.
    if (
      existing &&
      (existing.status === "opening" ||
        (existing.status === "open" && !opts.refresh))
    ) {
      return;
    }
    if (existing) this.tearDown(existing.conversationId);
    this.pendingOpens.add(tagma.tagma_id);
    try {
      await this.openRelay(tagma);
      this.openBudgets.delete(tagma.tagma_id);
    } catch (e) {
      this.recordOpenFailure(tagma.tagma_id, e);
      // Degraded offline view: hydrate the cached transcript so the chat
      // page renders history and accepts retryable sends instead of a
      // dead-end placeholder (no-op when nothing ever opened).
      void this.attachOfflineView(tagma.tagma_id);
      console.warn(
        `[channels] auto-open failed for tagma ${tagma.tagma_id}:`,
        e instanceof Error ? e.message : e,
      );
    } finally {
      this.pendingOpens.delete(tagma.tagma_id);
    }
  }

  /** Mount the degraded offline view for a tagma whose open just failed,
   *  hydrating the cached tail so history renders. No-op when a conversation
   *  already exists or this device has never opened the tagma (no
   *  remembered conversation id to hydrate from). */
  async attachOfflineView(tagmaId: string): Promise<void> {
    for (const conv of this.conversations.values()) {
      if (conv instanceof OfflineConversation && conv.tagmaId === tagmaId) {
        return;
      }
    }
    const conversationId = lastConversationOf(tagmaId);
    if (!conversationId) return;
    const user = archeionSession.user;
    const userId = user?.user_id;
    if (!userId) return;
    const conv = new OfflineConversation(conversationId, this, tagmaId, {
      kind: "user",
      id: userId,
      handle: user?.display_name ?? user?.username ?? userId,
    });
    this.conversations.set(conversationId, conv);
    // Hydrate the cached tail so the offline view renders history (the
    // same per-tagma cache the live conversation reads; a blocked cache
    // just leaves an empty transcript, sends still land in the store).
    try {
      const cached = await readTail(conversationId, WINDOW_PAGE);
      if (cached.length > 0) {
        conv.transcript = {
          lines: cached.map(cachedLineToLine),
          status: "idle",
        };
        conv.minRendered = cached[0]!.historyId;
        conv.maxRendered = cached[cached.length - 1]!.historyId;
      }
    } catch {
      // IndexedDB unavailable (private mode): proceed empty.
    }
  }

  /** Cold-start offline degrade (direct): the boot connect failed, so no
   *  local conversation exists -- mount the offline view from the
   *  remembered cache id instead of leaving the chat page on its
   *  connecting placeholder, hydrating the cached tail. localError stays
   *  set (the shell signals it). */
  async attachLocalOfflineView(): Promise<void> {
    if (this.local) return;
    const conversationId = lastConversationOf("local") ?? "local";
    const conv = new OfflineConversation(
      conversationId,
      this,
      "local",
      LOCAL_OPERATOR_SENDER,
    );
    this.conversations.set("local", conv);
    // Hydrate the cached tail (same shape as the tagma offline view; a
    // blocked cache just leaves an empty transcript, sends still land).
    try {
      const cached = await readTail(conversationId, WINDOW_PAGE);
      if (cached.length > 0) {
        conv.transcript = {
          lines: cached.map(cachedLineToLine),
          status: "idle",
        };
        conv.minRendered = cached[0]!.historyId;
        conv.maxRendered = cached[cached.length - 1]!.historyId;
      }
    } catch {
      // IndexedDB unavailable (private mode): proceed empty.
    }
  }

  /** The mounted degraded offline view for a tagma, if any (the chat page
   *  renders it in place of the dead-end unavailable placeholder). */
  offlineViewOf(tagmaId: string): OfflineConversation | undefined {
    const id = lastConversationOf(tagmaId);
    if (!id) return undefined;
    const conv = this.conversations.get(id);
    return conv instanceof OfflineConversation ? conv : undefined;
  }

  private recordOpenFailure(tagmaId: string, e: unknown): void {
    const failures = (this.openBudgets.get(tagmaId)?.failures ?? 0) + 1;
    const delay = Math.min(
      OPEN_BACKOFF_BASE_MS * 2 ** (failures - 1),
      OPEN_BACKOFF_CAP_MS,
    );
    this.openBudgets.set(tagmaId, {
      failures,
      cooldownUntil: this.now() + delay,
      terminal: failures >= OPEN_FAILURE_LIMIT,
    });
    if (e instanceof LescheApiError && e.status === 503) {
      this.offlineCorrection?.(tagmaId);
    }
  }

  /** True when the last open attempt failed and none is in flight (the
   *  chat page swaps its opening placeholder for the unavailable + retry
   *  row). Covers the session terminal too: a budget entry always exists
   *  by the time the automatic path gives up. */
  isAutoOpenFailed(tagmaId: string): boolean {
    return this.openBudgets.has(tagmaId);
  }

  /** User-initiated retry from a terminal row (the chat page's, or the
   *  manage page's): look the tagma
   *  up in the registry (it may have been revoked since) and open it
   *  explicitly, ignoring the failure budget. */
  retryTagma(tagmaId: string): void {
    const tagma = archeionSession.tagmata.find(
      (t) => t.tagma_id === tagmaId && t.state === "enrolled",
    );
    if (tagma) void this.ensureOpen(tagma, { explicit: true });
  }

  /** Clock seam for the budget's cooldown math; overridable in tests. */
  protected now(): number {
    return Date.now();
  }
  /** Send a prompt to a conversation. Renders the optimistic line and hands off
   *  to the conversation's send path (single-in-flight pump for relay; inline
   *  POST for local). */
  send(
    conversationId: string,
    text: string,
    attachment?: FileAttachment,
  ): void {
    const conv = this.conversations.get(conversationId);
    if (!conv || !conv.connected) return;
    void conv.send(text, attachment);
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
    conv.close();
    this.dropConv(conversationId);
  }

  /** Tear down every conversation (relay + local): close transports + drop
   *  entries, PRESERVING the IndexedDB cache. Used by mode switches, so the
   *  other mode rehydrates instantly from the shared cache on re-attach. */
  tearDownAll(): void {
    for (const conv of this.conversations.values()) {
      conv.close();
    }
    this.conversations.clear();
    this.tagmaIndex.clear();
    this.pendingOpens.clear();
    // The failure budget is session-scoped: a logout or mode switch ends
    // the session, so the next one must not inherit the previous one's
    // cooldown/terminal state.
    this.openBudgets.clear();
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
    // Global by design: run once, unconditionally (an empty-conversation
    // logout must still purge the pending store).
    void clearPendings();
    // The persisted conversation-map keys follow the same logout
    // contract: no session trace stays (convOfKey("") is the shared
    // key prefix).
    try {
      for (let i = localStorage.length - 1; i >= 0; i--) {
        const key = localStorage.key(i);
        if (key?.startsWith(convOfKey(""))) localStorage.removeItem(key);
      }
    } catch {
      // storage blocked: nothing to purge
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
    const conv = this.conversations.get(envelope.channel_id);
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
