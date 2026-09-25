<script lang="ts">
  import { page } from "$app/state";
  import { TagmaProfilePage } from "@kallipai/kallipai-ui";
</script>

<TagmaProfilePage tagmaId={page.params.id ?? ""} />
