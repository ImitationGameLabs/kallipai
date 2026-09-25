<script lang="ts">
  import { page } from "$app/state";
  import { UserProfilePage } from "@kallipai/kallipai-ui";
</script>

<UserProfilePage handle={page.params.handle ?? ""} />
