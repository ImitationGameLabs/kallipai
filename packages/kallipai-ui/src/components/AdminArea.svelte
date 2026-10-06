<script lang="ts">
  // The /admin subtree's gate: the children render only for an online
  // local-admin session; every other signed-in posture gets the 403
  // note card. Signed-out visits and offline mode never reach this
  // far -- the shell route's gate redirects both upstream.
  import { archeionSession } from "../lib/session/archeion.svelte.ts";
  import { shellMode } from "../lib/shell/port.ts";
  import { admin_area_forbidden } from "../paraglide/messages.js";
  import type { Snippet } from "svelte";

  let { children }: { children: Snippet } = $props();

  const authorized = $derived(
    shellMode() === "online" && archeionSession.user?.local_admin === true,
  );
</script>

{#if authorized}
  {@render children()}
{:else}
  <div class="px-2 md:p-6 max-w-3xl">
    <div class="card preset-tonal-surface p-4">
      <p class="text-sm text-error-500" role="status">
        {admin_area_forbidden()}
      </p>
    </div>
  </div>
{/if}
