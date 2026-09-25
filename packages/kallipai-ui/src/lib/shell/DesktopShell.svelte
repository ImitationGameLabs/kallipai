<script lang="ts">
  import type { Snippet } from "svelte";
  import { Navigation } from "@skeletonlabs/skeleton-svelte";
  import type { NavSection } from "./links.ts";
  import type { ErrorView } from "../errors.ts";
  import { matchTrail } from "./breadcrumbs.ts";
  import Brand from "../../components/Brand.svelte";
  import Banner from "../../components/Banner.svelte";
  import Breadcrumbs from "../../components/Breadcrumbs.svelte";
  import NavLink from "../../components/NavLink.svelte";

  let {
    links,
    isActive,
    brand,
    status,
    pathname,
    error = null,
    children,
  }: {
    links: NavSection[];
    isActive: (href: string) => boolean;
    /** Optional chrome snippets. `brand` defaults to a "KallipAI" wordmark
     * and is shown only in the sidebar header; `status` (e.g. an account
     * menu) is shown only in the sidebar footer. */
    brand?: Snippet;
    status?: Snippet;
    /** The current pathname: the trail table (lib/shell/breadcrumbs.ts)
     * is keyed on it, so the bar needs no per-page wiring. */
    pathname: string;
    error?: ErrorView | null;
    children: Snippet;
  } = $props();

  // The one chrome bar's content, from the route table. Null off-table
  // (offline /local/*, public routes) -> no bar, matching the old
  // per-page mounts by construction rather than by convention.
  const trail = $derived(matchTrail(pathname));
</script>

<!--
  The desktop half of the shell pair: the sidebar plus the content column.
  AppShell mounts it only above the 48rem branch point, so the old
  `hidden md:grid` toggle is gone -- being mounted IS the desktop verdict,
  and no media query inside this file second-guesses it. Zero business
  wiring by contract: everything (links, active matcher, account menu,
  error view) arrives as props from the shared RootLayout layer.
-->
<div class="h-dvh grid grid-cols-[auto_1fr] overflow-hidden">
  <!-- The descendant variant bumps the Skeleton trigger-text past its
       default size so labels read at desktop scale. -->
  <Navigation
    layout="sidebar"
    class="grid grid-rows-[auto_1fr_auto] gap-4 [&_[data-part='trigger-text']]:text-lg"
  >
    <Navigation.Header>
      {#if brand}
        {@render brand()}
      {:else}
        <!-- The wordmark doubles as the home link: `/` is the panorama, so
        the sidebar header is the always-visible way back. -->
        <a href="/" class="px-2 block w-fit"><Brand /></a>
      {/if}
    </Navigation.Header>
    <Navigation.Content>
      <Navigation.Menu>
        <!-- The section iteration never reads section.hub -- that field is
             consumed solely by navSlots on the small-screen bar, so this
             tree stays identical whether a section carries a hub or not. -->
        {#each links as section, i (section.title ?? `untitled-${i}`)}
          {#if section.title}
            <div
              class="px-2 pt-2 flex items-center justify-between gap-2 min-w-0"
            >
              <h2
                class="text-xs font-semibold uppercase tracking-wider opacity-60"
              >
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
            <NavLink {item} {isActive} />
          {/each}
        {/each}
      </Navigation.Menu>
    </Navigation.Content>
    {#if status}
      <Navigation.Footer>
        {@render status()}
      </Navigation.Footer>
    {/if}
  </Navigation>

  <main class="flex flex-col min-h-0 min-w-0 overflow-hidden">
    {#if error}
      <Banner title={error.title} detail={error.detail} hint={error.hint} />
    {/if}
    {#if trail}
      <!-- The single breadcrumb bar: one row, one divider, the same
           height on every page that has a trail (the table owns the
           content; the chrome owns the shape). Desktop-only since the
           shell split: the mobile half has no bar tier. -->
      <div
        class="px-4 py-2 border-b border-surface-200-800 flex min-h-9 items-center"
      >
        <Breadcrumbs segments={trail} />
      </div>
    {/if}
    <div class="flex-1 min-h-0 overflow-hidden">
      {@render children()}
    </div>
  </main>
</div>
