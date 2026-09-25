<script lang="ts">
  // The /local landing page: the offline home. Small screens arrive from the
  // bottom bar's home cell. One card gathers the mode's surfaces: the chat
  // entry first, then a stronger divider, then the manage grid (the shared
  // ManageGrid component; ManageHubPage stays as the /local/manage hub
  // shape). Icons are imported directly here (PanoramaPage precedent: page
  // components already depend on @lucide/svelte).
  import { MessageSquare } from "@lucide/svelte";
  import HubRow from "../components/HubRow.svelte";
  import ManageGrid from "../components/ManageGrid.svelte";
  import { nav_home, nav_chat } from "../paraglide/messages.js";
</script>

<!-- pt: calc keeps the browser value (1rem) when the inset is 0 and adds the system-bar height under edge-to-edge; these hub pages have no shell top row of their own. -->
<div
  class="px-2 pt-[calc(1rem+env(safe-area-inset-top))] pb-4 md:p-6 max-w-2xl"
>
  <nav class="card preset-tonal-surface" aria-label={nav_home()}>
    <HubRow href="/local/chat" Icon={MessageSquare} label={nav_chat()} />
    <div class="border-t-2 border-surface-300-700"></div>
    <ManageGrid />
  </nav>
</div>
