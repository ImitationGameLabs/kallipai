<script lang="ts">
  // One conversation over a transport -- relayed (online, E2EE RelayChannel) or
  // local (offline, DirectTransport). The store resolves `conversationId` to a
  // Conversation (`"local"` for offline, a server-derived id for online); the
  // page owns the chrome (status header + a local-aware empty state) and
  // composes the shared <ConversationView> for the transcript body. The page is
  // fully mode-agnostic: status, transcript, and pending count are all read off
  // the Conversation, so online and offline render identically.
  import ConversationView from "../components/ConversationView.svelte";
  import QuoteBar from "../components/QuoteBar.svelte";
  import { timezoneSetting } from "../lib/time/stamp.svelte.ts";
  import TagmaStatusHeader from "../components/TagmaStatusHeader.svelte";
  import AttachmentBar from "../components/AttachmentBar.svelte";
  import { createComposer } from "../lib/composer.svelte.ts";
  import { saveBlob } from "../lib/saveBlob.ts";
  import { bindDraft } from "../lib/session/drafts.svelte.ts";
  import {
    OfflineConversation,
    RelayConversation,
  } from "../lib/session/conversation.svelte.ts";
  import { statusCardStore } from "../lib/session/statusCard.svelte.ts";
  import { OnlineBackend } from "../lib/manage/backend.ts";
  import {
    ManageRestClient,
    ProjectionClient,
  } from "@kallipai/kallipai-keryx-client";
  import { keryxBaseUrlOrFail } from "../lib/session/archeion.svelte.ts";
  import { managementBackend } from "../lib/manage/client.ts";
  import { conversationStorageKey } from "../lib/session/drafts.ts";
  import { channelsStore } from "../lib/session/channels.svelte";
  import { tagmaForConversation } from "../lib/session/convOf.ts";
  import { createUnresolvedGuard } from "../lib/session/unresolvedGuard.svelte.ts";
  import {
    chat_conversation_unknown,
    chat_offline_no_history,
  } from "../paraglide/messages.js";
  import { realtimeStore } from "../lib/session/realtime.svelte";
  import {
    filesClientOrFail,
    archeionSession,
  } from "../lib/session/archeion.svelte.ts";
  import {
    allReady,
    isTooLarge,
    newAttachment,
    type AttachmentItem,
  } from "../lib/attachments.ts";
  import { createAttachmentTray } from "../lib/attachmentTray.svelte.ts";
  import type { FileAttachment } from "@kallipai/kallipai-lesche-client";
  import { ConversationBase } from "../lib/session/conversation.svelte.ts";
  import { createQuotes } from "../lib/quotes.svelte.ts";
  import { unreadStore, tagmaKey } from "../lib/session/unread.svelte.ts";
  import { formatDateTime } from "../lib/tagmata.svelte";
  import { navigate } from "../lib/shell/port.ts";
  import {
    connect_connecting,
    chat_opening,
    chat_reconnecting,
    chat_go_tagmata,
    chat_title_local,
    chat_title_channel,
    chat_cached_until,
    chat_notice_local,
    chat_notice_offline,
  } from "../paraglide/messages.js";

  let {
    conversationId,
    statusHeaderMobile = false,
  }: {
    conversationId: string;
    /** This page's own status header is desktop-only (md+): on small
     * screens the shell's mobile top row carries the status line for
     * every chat route (RootLayout lifts it there), so the header
     * defaults to hidden below md and no route needs to opt out. */
    statusHeaderMobile?: boolean;
  } = $props();

  // Resolves to undefined only briefly: online while a channel's key exchange
  // runs, or offline on a /local/chat deep-link before the boot reconnect lands
  // (the gate routes a failed reconnect to /connect, so this is a short window).
  const conv = $derived(channelsStore.get(conversationId));
  const isLocal = $derived(conversationId === "local");
  // Cache-freshness stamp for the offline banner: the newest CONFIRMED
  // cached line's timestamp (in-flight and failed local lines are not
  // server rows), so a returning reader knows how stale the view is.
  const lastCachedAt = $derived(
    conv?.transcript.lines.findLast(
      (line) => line.status !== "sending" && line.status !== "failed",
    )?.createdAt,
  );

  // Viewing: the open chat page clears the badge for the tagma's
  // 1:1 conversation and reports the read cursor (markRead), so the
  // tagma's own count drops too -- on every device. Lines landing while
  // the page stays mounted advance the cursor (one coalesced PUT per window),
  // and cleanup lands the pending write before the viewing flag drops.
  // Local conversations (the offline shell) carry no unread badge.
  $effect(() => {
    if (!(conv instanceof RelayConversation)) return;
    unreadStore.enter(tagmaKey(conv.tagmaId));
    conv.markRead();
    return () => {
      // Leaving the page: land the cursor still inside the throttle window.
      conv.flushReadCursor();
      unreadStore.leaveTagma(conv.tagmaId);
    };
  });
  // The lazy-window pager runs on both transports; each conversation leaf
  // supplies its own page source behind the shared base loadOlder.
  const loadOlder = $derived(
    conv instanceof ConversationBase ? () => conv.loadOlder() : undefined,
  );
  const windowStates = $derived(
    conv instanceof ConversationBase
      ? { hasMoreOlder: conv.hasMoreOlder, loadingOlder: conv.loadingOlder }
      : {},
  );

  // --- quoting ---------------------------------------------------------

  // The quoting state machine (armed id, peeked answers, probes) lives in
  // the controller alongside the page's other headless pieces; the page
  // only forwards and reads.
  const quotes = createQuotes({ conversation: () => conv });

  // Quoting needs two capabilities: serve a probe (the relay asks over the
  // envelope, the direct path over the history GET's query string) and carry
  // an armed id on send (the relay again in the envelope, the direct path in
  // the POST body). This flag stands for both holding; it does not itself
  // gate the carry, which is unconditional at the send site -- only an armed
  // id can travel, and arming starts at the entry points this flag gates, so
  // withholding the flag is what keeps a probe-less leaf from quoting. The
  // degraded offline view has neither capability, so it gets neither the
  // composer's entry point nor the block's retry. A leaf added later inherits
  // both; one that can probe but not carry would show the entry and then drop
  // the reference without saying so, while one that cannot probe shows up as
  // a block stuck retryable instead of quoting silently missing.
  const canQuote = $derived(!(conv instanceof OfflineConversation));

  // Every quote id the window cannot resolve gets exactly one peek. The
  // block renders nothing until that answer lands: a placeholder would
  // have to claim deletion or failure before either is known.
  $effect(() => quotes.sync());

  // --- /chat/{id} deep-link dead-end guard -------------------------------
  // A channel for this id may never open (offline tagma, or a conversation
  // from a previous boot). Reverse-resolve the owning tagma from the
  // remembered conv-of keys and mount its degraded offline view, so the
  // page renders history instead of a permanent "opening" placeholder.
  // The banner carries the id it was decided for: the page instance survives a
  // conversation switch (only the route param changes), so an unkeyed flag
  // would paint the miss over the next conversation. What counts as decided,
  // and why a still-opening channel records nothing, is the guard's own
  // module doc.
  const unresolved = createUnresolvedGuard();
  $effect(() => {
    if (conv || isLocal) {
      unresolved.settle();
      return;
    }
    const tagmaId = tagmaForConversation(conversationId);
    if (tagmaId) {
      unresolved.settle();
      void channelsStore.attachOfflineView(tagmaId);
    } else if (channelsStore.anyChannelOpening) {
      // Still opening: drop any earlier verdict and wait for the set to drain,
      // which re-runs this effect and reaches a verdict then.
      unresolved.settle();
    } else {
      unresolved.markUnknown(conversationId);
    }
  });
  const unknownConversation = $derived(unresolved.unknown(conversationId));

  // --- attachments (relay 1:1 only; the file button stays off otherwise) ---
  // Picked files upload at once (shared-space put, then inbox delivery);
  // the send gate holds until every item is ready, and one submit fans out
  // one message per file with the composer text riding the first.
  //
  // The tray and the draft share one storage key (conversationStorageKey):
  // an upload that outlives a switch lands in the bucket it was picked for
  // and a send reads only the current bucket -- a record id minted for one
  // tagma cannot ride a message to another.

  const storageKey = $derived(
    conversationStorageKey(
      conversationId,
      conv instanceof RelayConversation ? conv.tagmaId : undefined,
    ),
  );
  const tray = createAttachmentTray(() => storageKey);
  const attachments = $derived(tray.current());
  let attachSeq = 0;

  function setAttachment(
    owner: string,
    id: string,
    patch: Partial<AttachmentItem>,
  ): void {
    tray.patch(owner, id, patch);
  }

  async function uploadOne(
    item: AttachmentItem,
    tagmaId: string,
    owner: string,
  ): Promise<void> {
    const username = archeionSession.user?.username;
    if (!username) {
      setAttachment(owner, item.id, { status: "failed" });
      return;
    }
    try {
      const client = filesClientOrFail();
      const put = await client.put(
        `/users/${username}/shared/${item.name}`,
        item.file,
      );
      // The message references the recipient's inbox copy -- the send
      // response's record id -- which the user keeps full read on (Row1).
      const sent = await client.send(put.record_id, { toTagma: tagmaId });
      setAttachment(owner, item.id, {
        status: "ready",
        recordId: sent.record_id,
      });
    } catch {
      setAttachment(owner, item.id, { status: "failed" });
    }
  }

  function onFilesPicked(files: File[]): void {
    if (!(conv instanceof RelayConversation)) return;
    // The bucket this pick belongs to, captured before any await: the reader
    // may leave this conversation while the uploads are still running.
    const owner = storageKey;
    for (const file of files) {
      const item = newAttachment(attachSeq++, file);
      tray.add(owner, item);
      // Over the client-side cap: no wire traffic, straight to too_large
      // (the server 413 remains the authority for tighter limits).
      if (isTooLarge(item)) {
        setAttachment(owner, item.id, { status: "too_large" });
      } else {
        void uploadOne(item, conv.tagmaId, owner);
      }
    }
  }

  function retryAttachment(id: string): void {
    const item = attachments.find((a) => a.id === id);
    if (!item || !(conv instanceof RelayConversation)) return;
    const owner = storageKey;
    setAttachment(owner, id, { status: "uploading" });
    void uploadOne(item, conv.tagmaId, owner);
  }

  function removeAttachment(id: string): void {
    tray.remove(storageKey, id);
  }

  // The card's download I/O: pull the referenced inbox copy (the user
  // keeps full read on it, Row1) and hand the bytes to the browser as a
  // named download. The bubble renders any failure inline.
  async function downloadAttachment(attachment: FileAttachment): Promise<void> {
    const bytes = await filesClientOrFail().get(attachment.record_id);
    saveBlob(new Blob([bytes]), attachment.name);
  }

  const composer = createComposer({
    send: (text) => {
      // The bucket this send belongs to, read once here so the clear below
      // lands on the draft the reader typed into. The body runs to
      // tray.clear(owner) with no await in between, so the capture states
      // the contract rather than repairing a live drift; onFilesPicked is
      // the arm where an await really does intervene.
      const owner = storageKey;
      // One send carries one quote: the first message, the one the text
      // rides. Captured up front and cleared here, so a second submit
      // cannot inherit the strip the user already spent.
      const quoteId = quotes.sendQuote();
      quotes.cancel();
      // One message per ready file; the composer text rides the first and
      // the rest go out attachment-only (an empty text with an attachment
      // is a real line on the wire and in the transcript).
      const ready: FileAttachment[] = [];
      for (const item of attachments) {
        if (item.recordId !== undefined) {
          ready.push({
            record_id: item.recordId,
            name: item.name,
            size: item.size,
          });
        }
      }
      const first = ready.shift();
      if (first === undefined) {
        channelsStore.send(conversationId, text, { quote: quoteId });
        return;
      }
      channelsStore.send(conversationId, text, {
        attachment: first,
        quote: quoteId,
      });
      for (const att of ready) {
        channelsStore.send(conversationId, "", { attachment: att });
      }
      tray.clear(owner);
    },
    // Busy is not a gate: send renders the optimistic line at once and POSTs as
    // soon as the previous POST's user_message frame lands (single-in-flight
    // pump, shared by both transports).
    canSubmit: () =>
      (conv?.status === "open" || conv?.status === "offline") &&
      allReady(attachments), // open = live sends; offline = queued-to-store
    // An attachment-only submit (empty draft) is allowed once every
    // upload is ready; with no attachments the non-empty rule holds.
    allowEmpty: () => attachments.length > 0 && allReady(attachments),
  });

  // Draft storage: the very key the tray uses, so a draft and its
  // attachments never disagree about which conversation they belong to.
  bindDraft(composer, () => storageKey);

  // Status-area placement: the user's chosen form of the status area --
  // top bar (default) or right sidebar -- persisted across reloads via
  // localStorage ("statusLayout": "side"|"top"). The EFFECTIVE placement
  // additionally requires lg+ (64rem -- Tailwind's default lg, the
  // project sets no custom screens), because the sidebar needs desktop
  // width. Below lg the page stays on the top bar regardless of the
  // toggle; the matchMedia listener re-evaluates on resizes and its
  // removal in the $effect cleanup prevents a leak on unmount.
  // Storage may be blocked (private mode / cookies denied): the choice
  // then defaults to the top bar, and toggling simply does not persist
  // (the LightSwitch storage guard).
  function readStoredSide(): boolean {
    try {
      return localStorage.getItem("statusLayout") === "side";
    } catch {
      return false;
    }
  }
  let sideWanted = $state(readStoredSide());
  const lgQuery = matchMedia("(min-width: 64rem)");
  // Seed from the query's current state: a "change" event only fires on
  // transitions, never for the state at subscribe time.
  let lgMatches = $state(lgQuery.matches);
  $effect(() => {
    const onChange = (event: MediaQueryListEvent) =>
      (lgMatches = event.matches);
    lgQuery.addEventListener("change", onChange);
    return () => lgQuery.removeEventListener("change", onChange);
  });
  const sideLayout = $derived(sideWanted && lgMatches);

  // Feed the status-card rows from whichever backend this conversation
  // implies: the relay channel when online, the direct tagma when local.
  // $effect cleanup detaches on unmount or conversation switch, stopping
  // both poll cadences.
  $effect(() => {
    if (!conv) return;
    try {
      if (conv instanceof RelayConversation) {
        // Reading status makes this effect re-run when the drain resurrects
        // the transport; connected guards the un-attached window so the
        // backend chain below cannot dereference a missing relay transport.
        void conv.status;
        if (!conv.connected) return;
        statusCardStore.attach(
          new OnlineBackend(
            new ManageRestClient(keryxBaseUrlOrFail()),
            conv.relayTransport.relayChannel.tagmaId,
            new ProjectionClient(keryxBaseUrlOrFail()),
            conv.relayTransport.relayChannel,
          ),
        );
      } else {
        statusCardStore.attach(managementBackend());
      }
    } catch {
      /* no backend for this conversation: the bar stays row-less */
    }
    return () => statusCardStore.suspend();
  });
  // Roster rows follow status events: each snapshot update -- the relay
  // `tagma_status` push online, the direct SSE drain offline -- nudges an
  // immediate status-card refresh, so busy/idle flips paint at once. The
  // store's own interval is only the reconciliation backstop.
  $effect(() => {
    void conv?.statusSnapshot;
    statusCardStore.nudge();
  });

  // Focus refresh (relay path): the aggregate counters ride meaningful
  // pushes and the 30s reconciliation; a tab-visible restore catches up
  // immediately with the cache GET instead of waiting for either. The
  // wire payload routes through the realtime store's single dispatch
  // boundary, so the header, the status card, and the dashboard map all
  // see the same snapshot. Transient failures just wait for the next
  // push or focus.
  $effect(() => {
    if (!(conv instanceof RelayConversation)) return;
    void conv.status;
    if (!conv.connected) return;
    const tagmaId = conv.relayTransport.relayChannel.tagmaId;
    let client: ProjectionClient;
    try {
      client = new ProjectionClient(keryxBaseUrlOrFail());
    } catch {
      return; // no keryx base URL: nothing to refresh
    }
    const onVisible = () => {
      if (document.visibilityState !== "visible") return;
      client
        .status(tagmaId)
        .then((resp) =>
          realtimeStore.ingestStatusSnapshot(tagmaId, resp.status),
        )
        .catch(() => {});
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => document.removeEventListener("visibilitychange", onVisible);
  });

  // Offline views send by design (lines land in the pending store for a
  // later auto-flush), so they are not a disabled state.
  const disabled = $derived(
    !conv || (conv.status !== "open" && conv.status !== "offline"),
  );
  const pendingCount = $derived(conv?.pending.length ?? 0);
</script>

<svelte:head
  ><title>{isLocal ? chat_title_local() : chat_title_channel()}</title
  ></svelte:head
>
<div class="h-full flex flex-col">
  <div class="flex-1 min-h-0">
    {#if !conv}
      {#if isLocal}
        <!-- Offline /local/chat before the boot reconnect lands: the boot
         connect retries once and gives up (staying on the connecting
         placeholder is this window's worst case, not a /connect bounce). -->
        <div class="h-full grid place-items-center p-6">
          <p class="text-sm opacity-60">{connect_connecting()}</p>
        </div>
      {:else}
        <!-- No open channel for this conversation yet. Channels auto-connect at
         boot and on presence transitions, so this is normally brief; the
         guard above also reverse-resolves a remembered tagma and mounts its
         degraded offline view, so a deep-linked offline tagma renders its
         cached history instead of a permanent placeholder. -->
        <div class="h-full grid place-items-center p-6">
          <div class="text-center flex flex-col gap-3 max-w-sm">
            <p class="text-sm opacity-80">
              {unknownConversation
                ? chat_conversation_unknown()
                : chat_opening()}
            </p>
            <button
              type="button"
              class="btn preset-outlined-surface-500 hover:preset-filled-surface-500 self-center"
              onclick={() => navigate("/tagmata")}
            >
              {chat_go_tagmata()}
            </button>
          </div>
        </div>
      {/if}
    {:else}
      <div class={sideLayout ? "flex flex-row h-full" : "flex flex-col h-full"}>
        <!-- contents keeps the header a direct flex child at md+ (a plain
         block wrapper would break the side aside's flex-item contract:
         order-last, w-80, h-full); below md the local route hides it
         because the shell top row owns it there. -->
        <div class={statusHeaderMobile ? "contents" : "hidden md:contents"}>
          <TagmaStatusHeader
            status={conv.statusSnapshot}
            agentRows={{
              rootRow: statusCardStore.rootRow,
              subRows: statusCardStore.subRows,
            }}
            facts={statusCardStore.upstreamFacts}
            {sideLayout}
            onToggleSide={() => {
              sideWanted = !sideWanted;
              try {
                localStorage.setItem(
                  "statusLayout",
                  sideWanted ? "side" : "top",
                );
              } catch {
                /* storage blocked: the choice lives for this session only */
              }
            }}
          />
        </div>
        <!-- The wrapper gives the transcript a flex child whose width can be
         zeroed (min-w-0) in the sidebar state; in the top-bar state it is
         a no-op flex column. -->
        <div
          class="flex-1 min-h-0 flex flex-col {sideLayout
            ? 'min-w-0'
            : ''} relative"
        >
          {#if conv.status === "reconnecting"}
            <!-- Silent SSE retry in progress (transport-level reconnect): the
             failure itself stays in the console. The overlay ignores pointer
             events so the transcript stays scrollable; the composer is
             disabled via status !== "open". No backdrop by design: the
             transcript stays fully readable while the reconnect runs. -->
            <div
              class="absolute inset-0 z-10 grid place-items-center pointer-events-none"
              aria-busy="true"
            >
              <div
                class="card preset-tonal-surface flex flex-col items-center gap-3 px-8 py-6"
              >
                <div
                  class="size-10 rounded-full border-4 border-surface-400-600 border-t-transparent animate-spin"
                ></div>
                <p class="text-sm opacity-80">{chat_reconnecting()}</p>
              </div>
            </div>
          {/if}
          <ConversationView
            lines={conv.transcript.lines}
            status={conv.transcript.status}
            error={conv.transcript.error}
            sessionKey={conversationId}
            {composer}
            {disabled}
            {pendingCount}
            onRetry={conv instanceof OfflineConversation
              ? undefined
              : (id) => conv?.retrySend(id)}
            {loadOlder}
            hasMoreOlder={windowStates.hasMoreOlder}
            loadingOlder={windowStates.loadingOlder}
            fileButton={conv instanceof RelayConversation
              ? { onFilesPicked }
              : undefined}
            {downloadAttachment}
            onQuote={canQuote ? quotes.quote : undefined}
            resolveQuote={quotes.resolve}
            onQuoteRetry={canQuote ? quotes.retry : undefined}
            onCancelQuote={() => quotes.cancel()}
          >
            {#snippet notice()}
              {#if conv.kind === "offline-view" && conv.transcript.lines.length === 0}
                <p class="text-xs opacity-70 text-center">
                  {chat_offline_no_history()}
                </p>
              {/if}
              {#if conv.status === "offline"}
                <p
                  class="text-xs text-error-500 dark:text-error-400 text-center"
                >
                  {#if isLocal}
                    {chat_notice_local()}
                  {:else}
                    {chat_notice_offline()}
                  {/if}
                </p>
                {#if lastCachedAt}
                  <p class="text-xs opacity-60 text-center">
                    {chat_cached_until({
                      time: formatDateTime(lastCachedAt, timezoneSetting.value),
                    })}
                  </p>
                {/if}
              {/if}
            {/snippet}
            {#snippet quoteBar()}
              {#if quotes.quotedView}
                <QuoteBar
                  author={quotes.quotedView.author}
                  preview={quotes.quotedView.preview}
                  onCancel={() => quotes.cancel()}
                />
              {/if}
            {/snippet}
            {#snippet attachmentBar()}
              <AttachmentBar
                items={attachments}
                onRetry={retryAttachment}
                onRemove={removeAttachment}
              />
            {/snippet}
          </ConversationView>
        </div>
      </div>
    {/if}
  </div>
</div>
