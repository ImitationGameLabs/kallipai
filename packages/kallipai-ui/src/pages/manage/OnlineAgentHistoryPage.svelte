<script lang="ts">
  // Online agent turn history: a thin shell over OnlineAgentScope, which
  // resolves the tagma's relay channel and hands the page a ready backend.
  import OnlineAgentScope from "./OnlineAgentScope.svelte";
  import AgentHistoryPage from "./AgentHistoryPage.svelte";
  import { tagmaDetailsPath } from "../../lib/shell/routes.ts";

  let { tagmaId, agentId }: { tagmaId: string; agentId: string } = $props();
</script>

<OnlineAgentScope {tagmaId}>
  {#snippet children({ backend })}
    <AgentHistoryPage
      id={agentId}
      basePath={tagmaDetailsPath(tagmaId)}
      {backend}
    />
  {/snippet}
</OnlineAgentScope>
