<script lang="ts">
  import { page } from "$app/state";
  import { TagmaProfilePage } from "@kallipai/kallip-ui";
</script>

<TagmaProfilePage tagmaId={page.params.id ?? ""} />
