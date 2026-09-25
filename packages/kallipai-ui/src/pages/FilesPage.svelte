<script lang="ts">
  // The user-scope files page (/files): a thin title shell around the
  // dashboard, which owns every store call and all I/O (the RoomsPage
  // page/dashboard discipline). The root scrolls itself (the AppShell
  // expectation).
  import FilesDashboard from "../components/files/FilesDashboard.svelte";
  import { files_heading } from "../paraglide/messages.js";
</script>

<svelte:head>
  <title>{files_heading()}</title>
</svelte:head>

<div class="h-full overflow-y-auto">
  <div class="px-2 py-4 md:p-6 max-w-6xl mx-auto">
    <h1 class="text-xl font-semibold mb-4 hidden md:block">
      {files_heading()}
    </h1>
    <FilesDashboard />
  </div>
</div>
