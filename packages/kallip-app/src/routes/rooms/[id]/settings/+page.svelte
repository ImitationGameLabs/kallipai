<script lang="ts">
  import { page } from "$app/state";
  import { RoomSettingsPage } from "@kallipai/kallip-ui";
</script>

<RoomSettingsPage roomId={page.params.id ?? ""} />
