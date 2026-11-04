<script lang="ts">
  import { OnlineAgentHistoryPage } from "@kallipai/kallipai-ui";
  import { page } from "$app/state";
</script>

<OnlineAgentHistoryPage
  tagmaId={page.params.id ?? ""}
  agentId={page.params.agentId ?? ""}
/>
