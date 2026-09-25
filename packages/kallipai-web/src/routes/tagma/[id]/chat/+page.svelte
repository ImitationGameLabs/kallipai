<script lang="ts">
  import { page } from "$app/state";
  import { TagmaChatPage } from "@kallipai/kallipai-ui";
</script>

<TagmaChatPage tagmaId={page.params.id ?? ""} />
