<script lang="ts">
  import { page } from "$app/state";
  import { OnlineManagePage } from "@kallipai/kallip-ui";
</script>

<OnlineManagePage tagmaId={page.params.id ?? ""} page="tasks" />
