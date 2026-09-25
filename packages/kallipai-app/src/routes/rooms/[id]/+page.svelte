<script lang="ts">
  import { page } from "$app/state";
  import { RoomConversationPage } from "@kallipai/kallipai-ui";
</script>

<RoomConversationPage roomId={page.params.id ?? ""} />
