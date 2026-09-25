<script lang="ts">
  import { page } from "$app/state";
  import { ChannelChatPage } from "@kallipai/kallipai-ui";
</script>

<ChannelChatPage conversationId={page.params.id ?? ""} />
