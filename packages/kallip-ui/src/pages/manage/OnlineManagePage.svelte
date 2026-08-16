<script lang="ts">
  // Online management wrapper: resolves the RelayChannel for the given tagma,
  // creates an OnlineBackend, switches all stores to it, and renders the
  // management sub-page. The basePath is derived from the tagmaId so all
  // internal links stay within the /chat/t/{tagmaId}/manage/* tree.
  //
  // The stores share the RelayChannel with the chat — manage_result replies are
  // intercepted in RelayChannel.enqueue() and never reach the chat stream.

  import { onMount } from "svelte";
  import { channelsStore } from "../../lib/session/channels.svelte.ts";
  import { OnlineBackend } from "../../lib/manage/backend.ts";
  import { budgetStore } from "../../lib/manage/budget.svelte.ts";
  import { agentsStore } from "../../lib/manage/agents.svelte.ts";
  import { profilesStore } from "../../lib/manage/profiles.svelte.ts";
  import { schedulesStore } from "../../lib/manage/schedules.svelte.ts";
  import { managementBackend } from "../../lib/manage/client.ts";
  import OverviewPage from "./OverviewPage.svelte";
  import BudgetPage from "./BudgetPage.svelte";
  import AgentsPage from "./AgentsPage.svelte";
  import ProfilesPage from "./ProfilesPage.svelte";
  import SchedulesPage from "./SchedulesPage.svelte";

  let { tagmaId, page }: {
    tagmaId: string;
    page: "overview" | "budget" | "agents" | "profiles" | "schedules";
  } = $props();

  const basePath = `/chat/t/${tagmaId}/manage`;

  const channelState = $derived(channelsStore.getTagmaChannelState(tagmaId));
  const conversationId = $derived(
    channelState.kind === "open" || channelState.kind === "offline" || channelState.kind === "error"
      ? channelState.conversationId ?? null
      : null,
  );

  let backendReady = $state(false);
  let error = $state<string | null>(null);

  $effect(() => {
    if (!conversationId) {
      backendReady = false;
      return;
    }
    const conv = channelsStore.get(conversationId);
    if (!conv || conv.kind !== "relay") {
      backendReady = false;
      return;
    }
    try {
      // conv.kind === "relay" narrows to RelayConversation
      const relayConv = conv as import("../../lib/session/conversation.svelte.ts").RelayConversation;
      const channel = relayConv.relayTransport.relayChannel;
      const backend = new OnlineBackend(channel);
      budgetStore.switchBackend(backend);
      agentsStore.switchBackend(backend);
      profilesStore.switchBackend(backend);
      schedulesStore.switchBackend(backend);
      backendReady = true;
      error = null;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
      backendReady = false;
    }
  });

  onMount(() => {
    return () => {
      try {
        const b = managementBackend();
        budgetStore.switchBackend(b);
        agentsStore.switchBackend(b);
        profilesStore.switchBackend(b);
        schedulesStore.switchBackend(b);
      } catch { /* no offline config */ }
    };
  });
</script>

{#if !backendReady}
  <div class="h-full grid place-items-center p-6">
    <div class="text-center flex flex-col gap-3 max-w-sm">
      {#if error}
        <p class="text-error-500 dark:text-error-400 text-sm">{error}</p>
      {:else}
        <p class="text-sm opacity-60">Opening management channel…</p>
      {/if}
    </div>
  </div>
{:else if page === "overview"}
  <OverviewPage {basePath} />
{:else if page === "budget"}
  <BudgetPage {basePath} />
{:else if page === "agents"}
  <AgentsPage {basePath} />
{:else if page === "profiles"}
  <ProfilesPage {basePath} />
{:else if page === "schedules"}
  <SchedulesPage {basePath} />
{/if}
