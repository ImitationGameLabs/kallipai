<script lang="ts">
  import { AgentHistoryPage } from "@kallipai/kallipai-ui";
  import { page } from "$app/state";

  const id = $derived(page.params.id ?? "");
</script>

<AgentHistoryPage {id} />
