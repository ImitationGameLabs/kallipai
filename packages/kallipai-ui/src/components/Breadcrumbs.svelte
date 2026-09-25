<script lang="ts" module>
  // One trail segment: `href` makes it a link, and the current segment
  // (normally the tail) renders aria-current="page" instead of a link.
  // The type lives in the route table (lib/shell/breadcrumbs.ts), which
  // owns the trail data; re-exported here for this component's call sites.
  import type { BreadcrumbSegment } from "../lib/shell/breadcrumbs.ts";

  export type { BreadcrumbSegment };
</script>

<script lang="ts">
  import { ChevronRight } from "@lucide/svelte";
  import { nav_breadcrumbs_aria } from "../paraglide/messages.js";

  // Breadcrumb trail per the Skeleton v5 cookbook recipe (nav > list, chevron
  // separators, aria-current on the current segment). Labels and hrefs come
  // from the call site -- hrefs from the path-builder (lib/shell/routes.ts),
  // labels from the i18n vocabulary.
  let { segments }: { segments: BreadcrumbSegment[] } = $props();
</script>

<nav aria-label={nav_breadcrumbs_aria()}>
  <ol class="flex items-center gap-1 text-sm min-w-0">
    {#each segments as seg, i}
      <li class="flex items-center gap-1 min-w-0">
        {#if i > 0}
          <ChevronRight class="size-3 shrink-0 opacity-50" aria-hidden="true" />
        {/if}
        {#if seg.current}
          <span aria-current="page" class="font-semibold truncate"
            >{seg.label}</span
          >
        {:else if seg.href}
          <a href={seg.href} class="opacity-80 hover:underline truncate"
            >{seg.label}</a
          >
        {:else}
          <span class="opacity-80 truncate">{seg.label}</span>
        {/if}
      </li>
    {/each}
  </ol>
</nav>
