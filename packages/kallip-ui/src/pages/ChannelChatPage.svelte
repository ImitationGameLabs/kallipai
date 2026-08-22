<script lang="ts">
  // One conversation over a transport -- relayed (online, E2EE RelayChannel) or
  // local (offline, DirectTransport). The store resolves `conversationId` to a
  // Conversation (`"local"` for offline, a server-derived id for online); the
  // page owns the chrome (status header + a local-aware empty state) and
  // composes the shared <ConversationView> for the transcript body. The page is
  // fully mode-agnostic: status, transcript, and pending count are all read off
  // the Conversation, so online and offline render identically.
  import ConversationView from "../components/ConversationView.svelte";
  import TagmaStatusHeader from "../components/TagmaStatusHeader.svelte";
  import { createComposer } from "../lib/composer.svelte.ts";
  import { bindDraft } from "../lib/session/drafts.svelte.ts";
  import { RelayConversation } from "../lib/session/conversation.svelte.ts";
  import { statusCardStore } from "../lib/session/statusCard.svelte.ts";
  import { OnlineBackend } from "../lib/manage/backend.ts";
  import { managementBackend } from "../lib/manage/client.ts";
  import { convDraftKey, tagmaDraftKey } from "../lib/session/drafts.ts";
  import { channelsStore } from "../lib/session/channels.svelte";
  import { navigate } from "../lib/shell/port.ts";
  import {
    connect_connecting,
    chat_opening,
    chat_go_tagmata,
    chat_title_local,
    chat_title_channel,
    chat_notice_local,
    chat_notice_offline,
  } from "../paraglide/messages.js";

  let {
    conversationId,
    statusHeaderMobile = true,
  }: {
    conversationId: string;
    /** Keep this page's own status header below md. The offline /local/chat
     * route lifts it into the shell's mobile top row instead (RootLayout
     * renders a second instance there) and passes false here. */
    statusHeaderMobile?: boolean;
  } = $props();

  // Resolves to undefined only briefly: online while a channel's key exchange
  // runs, or offline on a /local/chat deep-link before the boot reconnect lands
  // (the gate routes a failed reconnect to /connect, so this is a short window).
  const conv = $derived(channelsStore.get(conversationId));
  const isLocal = $derived(conversationId === "local");

  const composer = createComposer({
    send: (text) => channelsStore.send(conversationId, text),
    // Busy is not a gate: send renders the optimistic line at once and POSTs as
    // soon as the previous POST's user_message frame lands (single-in-flight
    // pump, shared by both transports).
    canSubmit: () => conv?.status === "open",
  });

  // Draft storage: tagma chats key on the tagma id -- stable across re-KEX
  // and shared by both entries into this page (the sidebar /chat/t/{tagmaId}
  // route and a /chat/{conversationId} deep link resolve to the same
  // conversation). The local chat and the brief window before `conv`
  // resolves key on the conversation id, so no draft leaks across
  // conversations.
  const draftKey = $derived(
    conv instanceof RelayConversation
      ? tagmaDraftKey(conv.tagmaId)
      : convDraftKey(conversationId),
  );
  bindDraft(composer, () => draftKey);

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
        statusCardStore.attach(
          new OnlineBackend(conv.relayTransport.relayChannel),
        );
      } else {
        statusCardStore.attach(managementBackend());
      }
    } catch {
      /* no backend for this conversation: the bar stays row-less */
    }
    return () => statusCardStore.detach();
  });

  const disabled = $derived(!conv || conv.status !== "open");
  const pendingCount = $derived(conv?.pending.length ?? 0);
</script>

<svelte:head
  ><title>{isLocal ? chat_title_local() : chat_title_channel()}</title
  ></svelte:head
>

{#if !conv}
  {#if isLocal}
    <!-- Offline /local/chat before the boot reconnect lands. The gate routes a
         failed reconnect to /connect; this is the brief resolving window. -->
    <div class="h-full grid place-items-center p-6">
      <p class="text-sm opacity-60">{connect_connecting()}</p>
    </div>
  {:else}
    <!-- No open channel for this conversation yet. Channels auto-connect at
         boot and on presence transitions, so this is normally brief. The
         conversationId is server-derived and not reverse-resolvable, so if
         auto-connect does not open it (bogus id, revoked, offline tagma) the
         user needs a way out. -->
    <div class="h-full grid place-items-center p-6">
      <div class="text-center flex flex-col gap-3 max-w-sm">
        <p class="text-sm opacity-80">{chat_opening()}</p>
        <button
          type="button"
          class="btn preset-tonal-surface self-center"
          onclick={() => navigate("/tagmata")}
        >
          {chat_go_tagmata()}
        </button>
      </div>
    </div>
  {/if}
{:else}
  <div class={sideLayout ? "flex flex-row h-full" : "flex flex-col h-full"}>
    <!-- contents keeps the header a direct flex child on mobile; the local
         route hides it below md because the shell top row owns it there. -->
    <div class={statusHeaderMobile ? "contents" : "hidden md:block"}>
      <TagmaStatusHeader
      status={conv.statusSnapshot}
      agentRows={{
        rootRow: statusCardStore.rootRow,
        subRows: statusCardStore.subRows,
      }}
      {sideLayout}
      onToggleSide={() => {
        sideWanted = !sideWanted;
        try {
          localStorage.setItem("statusLayout", sideWanted ? "side" : "top");
        } catch {
          /* storage blocked: the choice lives for this session only */
        }
      }}
    />
    </div>
    <!-- The wrapper gives the transcript a flex child whose width can be
         zeroed (min-w-0) in the sidebar state; in the top-bar state it is
         a no-op flex column. -->
    <div class="flex-1 min-h-0 flex flex-col {sideLayout ? 'min-w-0' : ''}">
      <ConversationView
        lines={conv.transcript.lines}
        status={conv.transcript.status}
        error={conv.transcript.error}
        {composer}
        {disabled}
        {pendingCount}
      >
        {#snippet notice()}
          {#if conv.status === "offline"}
            <p class="text-xs text-error-500 dark:text-error-400 text-center">
              {#if isLocal}
                {chat_notice_local()}
              {:else}
                {chat_notice_offline()}
              {/if}
            </p>
          {/if}
        {/snippet}
      </ConversationView>
    </div>
  </div>
{/if}
