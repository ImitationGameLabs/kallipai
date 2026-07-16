<script lang="ts">
  import type { Snippet } from "svelte";
  import { Navigation } from "@skeletonlabs/skeleton-svelte";
  import type { NavItem } from "../lib/shell.ts";
  import type { ErrorView } from "../lib/errors.ts";

  let {
    links,
    isActive,
    brand,
    status,
    error = null,
    children,
  }: {
    links: NavItem[];
    // Consumer-supplied route matcher ("/" exact, others by prefix). Kept out of
    // the shell so deep links such as `/approvals/:id` still resolve.
    isActive: (href: string) => boolean;
    // Optional chrome snippets. `brand` defaults to a "KallipAI" wordmark and is
    // shown only in the sidebar header; `status` (e.g. a connection pill) is
    // shown only in the sidebar footer. Both are omitted on the bar tier to keep
    // the compact bottom navigation clean.
    brand?: Snippet;
    status?: Snippet;
    // Rendered as a uniform banner above the page content.
    error?: ErrorView | null;
    children: Snippet;
  } = $props();
</script>

{#snippet navLinks()}
  {#each links as item (item.href)}
    {@const active = isActive(item.href)}
    {@const Icon = item.icon}
    <Navigation.TriggerAnchor
      href={item.href}
      aria-current={active ? "page" : undefined}
      class={active
        ? "preset-filled-primary-500 hover:preset-filled-primary-500"
        : "preset-tonal-surface"}
    >
      {#if Icon}<Icon class="size-4" />{/if}
      <Navigation.TriggerText>{item.label}</Navigation.TriggerText>
    </Navigation.TriggerAnchor>
  {/each}
{/snippet}

<!--
  Responsive shell: two Skeleton `Navigation` instances — a bottom `bar` on
  small viewports and a `sidebar` from `md` up — toggled by a single Tailwind
  breakpoint. Both are safe to render together because Skeleton's Navigation is
  stateless (no machine, no generated IDs); the hidden one uses `display:none`,
  which also drops it from the a11y tree and tab order, so exactly one nav is
  exposed at any width. Skeleton's `[data-part='root']` sets no `display` on the
  bar/sidebar layouts (only the rail layout did, which we don't use), so the
  `md:grid` / `md:hidden` utilities below are the sole source of the layout-box
  display and win trivially.

  The sidebar root is overridden to `grid` (to lay out header/content/footer
  rows); the bar stays in its default block flow.
-->
<div
  class="h-dvh grid grid-rows-[1fr_auto] md:grid-cols-[auto_1fr] md:grid-rows-1 overflow-hidden"
>
  <!-- sidebar (md and up). The descendant variant bumps the Skeleton
       trigger-text past its default size so labels read at desktop scale (the
       bar keeps Skeleton's compact sizing). -->
  <Navigation
    layout="sidebar"
    class="hidden md:grid grid-rows-[auto_1fr_auto] gap-4 [&_[data-part='trigger-text']]:text-lg"
  >
    <Navigation.Header>
      {#if brand}
        {@render brand()}
      {:else}
        <span class="px-2 text-xl font-bold tracking-tight">KallipAI</span>
      {/if}
    </Navigation.Header>
    <Navigation.Content>
      <Navigation.Menu>
        {@render navLinks()}
      </Navigation.Menu>
    </Navigation.Content>
    {#if status}
      <Navigation.Footer>
        {@render status()}
      </Navigation.Footer>
    {/if}
  </Navigation>

  <!-- page content -->
  <main class="flex flex-col min-h-0 min-w-0 overflow-hidden">
    {#if error}
      <div
        role="alert"
        class="px-4 py-2 text-sm bg-error-500/10 text-error-500 flex flex-wrap items-center gap-x-2 gap-y-1"
      >
        <span class="font-medium">{error.title}</span>
        {#if error.detail}<span class="opacity-80">{error.detail}</span>{/if}
        {#if error.hint}<span class="opacity-60">{error.hint}</span>{/if}
      </div>
    {/if}
    <div class="flex-1 min-h-0 overflow-hidden">
      {@render children()}
    </div>
  </main>

  <!-- small: bottom bar. The bottom padding follows the safe-area inset, which
       is non-zero only when the webview is edge-to-edge; it collapses to 0
       otherwise (e.g. Tauri Android's default, non-edge-to-edge webview). -->
  <Navigation layout="bar" class="md:hidden pb-[env(safe-area-inset-bottom)]">
    <!-- Inline style because the column count is dynamic (links.length); a
         static grid-cols-N utility can't express it. -->
    <Navigation.Menu
      style="display:grid; grid-template-columns: repeat({links.length}, minmax(0, 1fr));"
    >
      {@render navLinks()}
    </Navigation.Menu>
  </Navigation>
</div>
