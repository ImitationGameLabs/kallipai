<script lang="ts">
  import { AgentDetailPage } from "@kallipai/kallip-ui";
  import { page } from "$app/state";

  const id = $derived(page.params.id ?? "");
</script>

<AgentDetailPage {id} />
