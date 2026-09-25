<script lang="ts">
  import { page } from "$app/state";
  import { OnlineManagePage } from "@kallipai/kallipai-ui";
</script>

<OnlineManagePage tagmaId={page.params.id ?? ""} page="overview" />
