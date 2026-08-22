<script lang="ts">
  import type { Snippet } from "svelte";
  import { Dialog, Navigation, Portal } from "@skeletonlabs/skeleton-svelte";
  import type { NavIndicator, NavItem } from "../lib/shell.ts";
  import { navSlots } from "../lib/shell/navSlots.ts";
  import { Ellipsis, User } from "@lucide/svelte";
  import type { NavSection } from "../lib/shell/links.ts";
  import type { ErrorView } from "../lib/errors.ts";
  import Brand from "./Brand.svelte";
  import Banner from "./Banner.svelte";
  import {
    account_menu,
    shell_connecting,
    nav_more,
    shell_error,
    shell_live,
    shell_offline,
  } from "../paraglide/messages.js";

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
        return "bg-surface-400-600";
      case "error":
        return "bg-error-500";
      // Unreachable at runtime: the snippet renders a spinner for "pending"
      // before reaching here. Kept so the switch stays exhaustive (required for
      // the return type); do NOT delete without restructuring.
      case "pending":
        return "bg-surface-400-600";
    }
  }

  // The dot itself is aria-hidden (decorative); this label carries the status
  // to screen readers so an SR user learns the channel's liveness, not just its
  // name. Rendered as visually-hidden text inside the anchor.
  function navIndicatorLabel(indicator: NavIndicator): string {
    switch (indicator) {
      case "live":
        return shell_live();
      case "pending":
        return shell_connecting();
      case "down":
        return shell_offline();
      case "error":
        return shell_error();
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
    // shown only in the sidebar header; `status` (e.g. an account menu) is
    // shown only in the sidebar footer. The bar tier keeps its cells compact
    // and instead renders an account cell (a navLink to /account, the
    // hub page) in the last slot, so account actions stay reachable below md.
    brand?: Snippet;
    status?: Snippet;
    // Rendered as a uniform banner above the page content.
    error?: ErrorView | null;
    children: Snippet;
  } = $props();

  // Small-viewport bar slot plan: cap visible nav cells, overflow the rest
  // into a bottom sheet opened by the More button (navSlots owns the
  // arithmetic; both modes stay <= 5 cells incl. More + Account).
  const slots = $derived(navSlots(links));
  const moreActive = $derived(slots.overflow.some((i) => isActive(i.href)));

  // The overflow sheet. Backdrop/Escape dismissal comes from the zag
  // Dialog; in-sheet navigation closes it via the menu-level click
  // delegate below. Crossing INTO md also closes it: the sheet portals
  // to body, so past the breakpoint it would float above a bar that no
  // longer renders (the ChannelChatPage matchMedia listener pattern).
  let sheetOpen = $state(false);
  const mdQuery = matchMedia("(min-width: 48rem)");
  $effect(() => {
    const onChange = (event: MediaQueryListEvent) => {
      if (event.matches) sheetOpen = false;
    };
    mdQuery.addEventListener("change", onChange);
    return () => mdQuery.removeEventListener("change", onChange);
  });
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
        class="size-2 rounded-full border-2 border-surface-400-600 border-t-transparent animate-spin shrink-0"
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
  <!-- Sidebar-only snippet: it iterates section.items and never reads
       section.hub — that field is consumed solely by navSlots on the
       small-screen bar, so this desktop tree stays identical whether a
       section carries a hub or not. -->
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
      <div class="border-b border-surface-200-800" role="separator"></div>
    {/if}
    {#each section.items as item (item.href)}
      {@render navLink(item)}
    {/each}
  {/each}
{/snippet}

{#snippet sheetSection(section: NavSection)}
  <!-- A sheet copy of one sidebar section: optional title row with its manage
       gear, then the overflow items. Purely additive (B-plan): the sidebar's
       navLinks snippet above stays untouched. Gear rows are enlarged to a
       40px touch target (the sidebar's size-5 is mouse-scale). -->
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
          class="size-10 grid place-items-center rounded-base opacity-50 hover:opacity-100 hover:preset-filled-surface-500 shrink-0"
        >
          <ManageIcon class="size-4" />
        </a>
      {/if}
    </div>
    <div class="border-b border-surface-200-800" role="separator"></div>
  {/if}
  {#each section.items as item (item.href)}
    {@render navLink(item)}
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

  <!-- small: bottom bar with capped cells. Visible nav items come from the
       slot plan (navSlots); overflow items and every section-manage gear
       live in the More sheet below. The trailing cell is always
       the account entry: a navLink to /account — the hub page that
       carries what the sidebar footer's dropdown serves at md+. The
       bottom padding follows the safe-area inset, which is
       non-zero only when the webview is edge-to-edge; it collapses to 0
       otherwise (e.g. Tauri Android's default, non-edge-to-edge webview). -->
  <Navigation layout="bar" class="md:hidden pb-[env(safe-area-inset-bottom)]">
    <!-- Inline style because the column count is dynamic (visible cells
         plus More plus Account); a static grid-cols-N utility can't
         express it. -->
    <Navigation.Menu
      style="display:grid; grid-template-columns: repeat({slots.visible.length +
        (slots.hasMore ? 1 : 0) +
        1}, minmax(0, 1fr));"
    >
      {#each slots.visible as item (item.href)}
        {@render navLink(item)}
      {/each}
      {#if slots.hasMore}
        <button
          type="button"
          onclick={() => (sheetOpen = true)}
          aria-label={nav_more()}
          aria-haspopup="dialog"
          class="size-10 justify-self-center self-center grid place-items-center rounded-base {moreActive
            ? 'preset-filled-surface-500'
            : 'preset-tonal-surface hover:preset-filled-surface-500'}"
        >
          <Ellipsis class="size-5" />
        </button>
      {/if}
      {@render navLink({
        href: "/account",
        label: account_menu(),
        icon: User,
      })}
    </Navigation.Menu>
  </Navigation>

  <!-- small: the overflow sheet. Portaled to body; the sheet body is wrapped
       in its own stateless Navigation (bar layout) because navLink's
       TriggerAnchor/TriggerText consume the Navigation root context and
       would throw outside a Navigation subtree. Any in-sheet anchor click
       (item or manage gear) closes the sheet. -->
  {#if slots.hasMore}
    <Dialog open={sheetOpen} onOpenChange={(e) => (sheetOpen = e.open)}>
      <Portal>
        <Dialog.Backdrop class="fixed inset-0 bg-surface-50-950/60 z-50" />
        <Dialog.Positioner class="fixed inset-0 z-50 grid items-end">
          <Dialog.Content
            class="card preset-tonal-surface w-full rounded-t-xl rounded-b-none p-4 pb-[max(1rem,env(safe-area-inset-bottom))] max-h-[80dvh] overflow-y-auto"
          >
            <Dialog.Title class="sr-only">{nav_more()}</Dialog.Title>
            <Navigation layout="bar">
              <Navigation.Menu
                onclick={(e) => {
                  if ((e.target as HTMLElement).closest("a")) sheetOpen = false;
                }}
              >
                {#each slots.sheetSections as section, i (section.title ?? `untitled-${i}`)}
                  {@render sheetSection(section)}
                {/each}
              </Navigation.Menu>
            </Navigation>
          </Dialog.Content>
        </Dialog.Positioner>
      </Portal>
    </Dialog>
  {/if}
</div>
