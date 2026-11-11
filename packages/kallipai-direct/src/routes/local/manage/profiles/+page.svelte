<script lang="ts">
  // Local-manage profiles page: the shared page body plus the shared
  // unsaved-changes guard. The guard registers kit's beforeNavigate, which
  // only a kit host can resolve, so the hook is injected here.
  import { beforeNavigate } from "$app/navigation";
  import { LeaveGuard, ProfilesPage } from "@kallipai/kallipai-ui";
</script>

<ProfilesPage />

<LeaveGuard {beforeNavigate} fallback={() => "/local/manage"} />
