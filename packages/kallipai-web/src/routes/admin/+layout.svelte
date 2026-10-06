<script lang="ts">
  import { AdminArea } from "@kallipai/kallipai-ui";

  let { children } = $props();
</script>

<!-- The /admin subtree renders only for online local admins; every
     other signed-in posture gets the 403 note before any page. -->
<AdminArea>
  {@render children()}
</AdminArea>
