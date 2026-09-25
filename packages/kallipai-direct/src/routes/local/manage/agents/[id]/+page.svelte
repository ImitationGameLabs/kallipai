<script lang="ts">
  import { AgentDetailPage } from "@kallipai/kallipai-ui";
  import { page } from "$app/state";

  const id = $derived(page.params.id ?? "");
</script>

<AgentDetailPage {id} />
