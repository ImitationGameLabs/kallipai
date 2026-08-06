<script lang="ts">
  import { page } from "$app/state";
  import { UserProfilePage } from "@kallipai/kallip-ui";
</script>

<UserProfilePage handle={page.params.handle ?? ""} />
