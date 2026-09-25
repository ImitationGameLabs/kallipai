<script lang="ts">
  import type { Snippet } from "svelte";

  // The shared top bar pages compose instead of inlining their own <header>
  // (extracted verbatim from RoomConversationPage's bar as the reuse proof).
  // A page passes its title block and, optionally, badges, actions, and a
  // breadcrumb trail (Breadcrumbs.svelte); absent snippets render nothing, so
  // the bar stays a single flex row; the details-tree trail mounts inside it.
  let {
    title,
    badges = undefined,
    actions = undefined,
    breadcrumbs = undefined,
  }: {
    title: Snippet;
    badges?: Snippet;
    actions?: Snippet;
    breadcrumbs?: Snippet;
  } = $props();
</script>

<header
  class="px-4 py-2 border-b border-surface-200-800 flex items-center gap-2"
>
  {#if breadcrumbs}
    {@render breadcrumbs()}
  {/if}
  <div class="flex flex-col min-w-0 flex-1">
    {@render title()}
  </div>
  {#if badges}
    {@render badges()}
  {/if}
  {#if actions}
    {@render actions()}
  {/if}
</header>
