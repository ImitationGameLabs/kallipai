<script lang="ts">
  import { page } from "$app/state";
  import { RoomSettingsPage } from "@kallipai/kallipai-ui";
</script>

<RoomSettingsPage roomId={page.params.id ?? ""} />
