<script lang="ts">
  import { beforeNavigate } from "$app/navigation";
  import { page } from "$app/state";
  import { LeaveGuard, RouteView } from "@kallipai/kallipai-ui";
</script>

<RouteView pathname={page.url.pathname} search={page.url.search} />

<LeaveGuard {beforeNavigate} fallback={() => "/chats"} />
