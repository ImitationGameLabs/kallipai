<script lang="ts">
  // The /local/manage landing page: one full-width row per management page.
  // This is the small-screen destination of the bottom bar's single manage
  // cell (links.ts marks that section with hub); the desktop sidebar keeps
  // its five direct links and simply never renders this page's entry. The
  // row list is the shared ManageGrid component; LocalHomePage's manage
  // section renders the same five rows.
  import ManageGrid from "../../components/ManageGrid.svelte";
  import { nav_manage } from "../../paraglide/messages.js";
</script>

<div class="px-2 py-4 md:p-6 max-w-2xl space-y-6">
  <h1 class="text-xl font-semibold">{nav_manage()}</h1>

  <nav class="card preset-tonal-surface" aria-label={nav_manage()}>
    <ManageGrid />
  </nav>
</div>
