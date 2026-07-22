<script lang="ts">
  import { page } from "$app/state";
  import { ChannelChatPage } from "@kallipai/kallip-ui";
</script>

<ChannelChatPage convId={page.params.id ?? ""} />
