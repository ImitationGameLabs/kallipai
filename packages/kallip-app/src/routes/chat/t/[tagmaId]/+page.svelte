<script lang="ts">
  import { page } from "$app/state";
  import { TagmaChatPage } from "@kallipai/kallip-ui";
</script>

<TagmaChatPage tagmaId={page.params.tagmaId ?? ""} />
