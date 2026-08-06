<script lang="ts">
  import type { Snippet } from "svelte";
  import { Navigation } from "@skeletonlabs/skeleton-svelte";
  import type { NavIndicator, NavItem } from "../lib/shell.ts";
  import type { NavSection } from "../lib/shell/links.ts";
  import type { ErrorView } from "../lib/errors.ts";
  import Brand from "./Brand.svelte";
  import Banner from "./Banner.svelte";

  // AppShell owns the indicator visual tokens (mirrors how it owns the icon's
  // `size-4`), so callers only express a domain tri-state, not a class string.
  // The `pending` ("connecting") case is rendered as a spinning ring in the
  // snippet (not here): a filled dot has no visible rotation axis, so a border
  // ring with a transparent segment reads as motion. This helper covers the
  // remaining filled-dot states.
  function navIndicatorDotClass(indicator: NavIndicator): string {
    switch (indicator) {
      case "live":
        return "bg-success-500";
      case "down":
        return "bg-surface-400";
      case "error":
        return "bg-error-500";
      // Unreachable at runtime: the snippet renders a spinner for "pending"
      // before reaching here. Kept so the switch stays exhaustive (required for
      // the return type); do NOT delete without restructuring.
      case "pending":
        return "bg-surface-400";
    }
  }

  // The dot itself is aria-hidden (decorative); this label carries the status
  // to screen readers so an SR user learns the channel's liveness, not just its
  // name. Rendered as visually-hidden text inside the anchor.
  function navIndicatorLabel(indicator: NavIndicator): string {
    switch (indicator) {
      case "live":
        return "live";
      case "pending":
        return "connecting";
      case "down":
        return "offline";
      case "error":
        return "error";
    }
  }

  let {
    links,
    isActive,
    brand,
    status,
    error = null,
    children,
  }: {
    links: NavSection[];
    // Consumer-supplied route matcher ("/" exact, others by prefix). Kept out of
    // the shell so deep links such as `/chat/:id` still resolve.
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

{#snippet navLink(item: NavItem)}
  {@const active = isActive(item.href)}
  {@const Icon = item.icon}
  {@const indicator = item.indicator}
  <Navigation.TriggerAnchor
    href={item.href}
    aria-current={active ? "page" : undefined}
    class={active
      ? "preset-filled-surface-500"
      : "preset-tonal-surface hover:preset-filled-surface-500"}
  >
    {#if indicator === "pending"}
      <!-- A spinning ring (not a filled dot): a filled dot has no visible
           rotation axis, so the border + transparent top segment reads as
           motion. Size-matched to the size-2 status dot. aria-hidden; the
           sr-only "connecting" label below carries state. -->
      <span
        class="size-2 rounded-full border-2 border-surface-400 border-t-transparent animate-spin shrink-0"
        aria-hidden="true"
      ></span>
    {:else if indicator}
      <span
        class="size-2 rounded-full shrink-0 {navIndicatorDotClass(indicator)}"
        aria-hidden="true"
      ></span>
    {:else if Icon}<Icon class="size-4" />{/if}
    <Navigation.TriggerText>{item.label}</Navigation.TriggerText>
    {#if indicator}
      <span class="sr-only">{navIndicatorLabel(indicator)}</span>
    {/if}
  </Navigation.TriggerAnchor>
{/snippet}

{#snippet navLinks()}
  {#each links as section, i (section.title ?? `untitled-${i}`)}
    {#if section.title}
      <div class="px-2 pt-2 flex items-center justify-between gap-2 min-w-0">
        <h2 class="text-xs font-semibold uppercase tracking-wider opacity-60">
          {section.title}
        </h2>
        {#if section.manage}
          {@const ManageIcon = section.manage.icon}
          <a
            href={section.manage.href}
            aria-label={section.manage.label}
            title={section.manage.label}
            class="size-5 grid place-items-center rounded-base opacity-50 hover:opacity-100 hover:preset-filled-surface-500 shrink-0"
          >
            <ManageIcon class="size-3.5" />
          </a>
        {/if}
      </div>
      <div class="border-b border-surface-200" role="separator"></div>
    {/if}
    {#each section.items as item (item.href)}
      {@render navLink(item)}
    {/each}
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
        <span class="px-2"><Brand /></span>
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
      <Banner title={error.title} detail={error.detail} hint={error.hint} />
    {/if}
    <div class="flex-1 min-h-0 overflow-hidden">
      {@render children()}
    </div>
  </main>

  <!-- small: bottom bar. The bottom padding follows the safe-area inset, which
       is non-zero only when the webview is edge-to-edge; it collapses to 0
       otherwise (e.g. Tauri Android's default, non-edge-to-edge webview). The
       bar is one flat row of items -- section headers/dividers are a sidebar
       concept, so items are flattened across sections here. -->
  <Navigation layout="bar" class="md:hidden pb-[env(safe-area-inset-bottom)]">
    <!-- Inline style because the column count is dynamic (the total item
         count); a static grid-cols-N utility can't express it. -->
    <Navigation.Menu
      style="display:grid; grid-template-columns: repeat({links.reduce(
        (n, s) => n + s.items.length,
        0,
      )}, minmax(0, 1fr));"
    >
      {#each links as section}
        {#each section.items as item (item.href)}
          {@render navLink(item)}
        {/each}
      {/each}
    </Navigation.Menu>
  </Navigation>
</div>
