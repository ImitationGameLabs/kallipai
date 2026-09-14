<script lang="ts">
  import type { Snippet } from "svelte";
  import { Dialog, Navigation, Portal } from "@skeletonlabs/skeleton-svelte";
  import { Ellipsis, User, ChevronLeft } from "@lucide/svelte";
  import type { NavSection } from "./links.ts";
  import type { ErrorView } from "../errors.ts";
  import { navSlots } from "./navSlots.ts";
  import { account_menu, nav_more } from "../../paraglide/messages.js";
  import Banner from "../../components/Banner.svelte";
  import NavLink from "../../components/NavLink.svelte";

  let {
    links,
    isActive,
    back = null,
    topRow = undefined,
    title = undefined,
    error = null,
    topPanel = undefined,
    children,
  }: {
    links: NavSection[];
    isActive: (href: string) => boolean;
    // The back affordance: when set, the top row renders the chevron and
    // the bottom bar is replaced by it (a deep page is a drill, not a
    // destination -- the target derivation lives in RootLayout, the
    // offline /local/* rule plus lib/shell/breadcrumbs.ts mobileBack).
    back?: { href: string; label: string } | null;
    // Small-screen top row: when set, renders beside the back affordance
    // above the banner (chat pages lift their status line here).
    topRow?: Snippet;
    // Fallback centre cell when no topRow snippet: a page title (manage
    // sub-pages map their static i18n heading here; the page keeps its
    // own h1 for md+). role=heading keeps the accessible name without a
    // second h1 in the tree.
    title?: string;
    error?: ErrorView | null;
    // Optional second row under the top row (the chat's expanded status
    // panel): full width, above the banner.
    topPanel?: Snippet;
    children: Snippet;
  } = $props();

  // Bar slot plan: cap visible nav cells, overflow the rest into a bottom
  // sheet opened by the More button (navSlots owns the arithmetic; both
  // modes stay <= 5 cells incl. More + Account).
  const slots = $derived(navSlots(links));
  const moreActive = $derived(slots.overflow.some((i) => isActive(i.href)));

  // The overflow sheet. Backdrop/Escape dismissal comes from the zag
  // Dialog; in-sheet navigation closes it via the menu-level click
  // delegate below. Crossing the breakpoint unmounts this whole shell
  // (AppShell's branch), which is the active-zeroing the old md listener
  // enforced: an unmounted sheet cannot float above a bar that is not
  // there, and a fresh mount starts closed.
  let sheetOpen = $state(false);
</script>

<!--
  The small-screen half of the shell pair: top row, optional status panel,
  banner, content, bottom bar and overflow sheet. AppShell mounts it only
  below the 48rem branch point, so no `md:hidden` guards remain inside.
  No breadcrumb bar here by design: on a phone the trail chain collapses
  into the back row. Zero business wiring by contract: everything arrives
  as props from the shared RootLayout layer.
-->
<div class="h-dvh grid grid-rows-[1fr_auto] overflow-hidden">
  <main class="flex flex-col min-h-0 min-w-0 overflow-hidden">
    {#if back || topRow || title}
      <!-- The mobile top row: the shell's back affordance plus the
           page-supplied status line (chat pages lift their
           TagmaStatusHeader here so the banner sorts below it). The grid
           keeps the centre column truly centred whether or not a back
           button exists. -->
      <div
        class="grid grid-cols-[auto_1fr_auto] items-center px-2 pt-[env(safe-area-inset-top)] min-h-10"
      >
        {#if back}
          <a
            href={back.href}
            aria-label={back.label}
            class="size-8 grid place-items-center rounded-base text-primary-500 dark:text-primary-400 hover:preset-filled-surface-500"
          >
            <ChevronLeft class="size-4" aria-hidden="true" />
          </a>
        {:else}
          <span class="size-8"></span>
        {/if}
        <div class="min-w-0 flex justify-center px-1">
          {#if topRow}
            {@render topRow()}
          {:else if title}
            <span
              role="heading"
              aria-level="1"
              class="text-sm font-semibold truncate">{title}</span
            >
          {/if}
        </div>
        <span class="size-8"></span>
      </div>
    {/if}
    {#if topPanel}
      <div>
        {@render topPanel()}
      </div>
    {/if}
    {#if error}
      <Banner title={error.title} detail={error.detail} hint={error.hint} />
    {/if}
    <div class="flex-1 min-h-0 overflow-hidden">
      {@render children()}
    </div>
  </main>
  <!-- Bottom bar with capped cells. Visible nav items come from the slot
       plan (navSlots); overflow items and every section-manage gear live
       in the More sheet below. The trailing cell is always the account
       entry: a navLink to /account -- the hub page that carries what the
       desktop sidebar footer's dropdown serves. The bottom padding
       follows the safe-area inset, which is non-zero only when the
       webview is edge-to-edge; it collapses to 0 otherwise (e.g. Tauri
       Android's default, non-edge-to-edge webview). -->
  {#if !back}
    <Navigation layout="bar" class="pb-[env(safe-area-inset-bottom)]">
      <!-- Inline style because the column count is dynamic (visible cells
           plus More plus Account); a static grid-cols-N utility can't
           express it. It sits inside the {#if !back} block above: on deep
           pages the back row replaces the whole bar. -->
      <Navigation.Menu
        style="display:grid; grid-template-columns: repeat({slots.visible
          .length +
          (slots.hasMore ? 1 : 0) +
          1}, minmax(0, 1fr));"
      >
        {#each slots.visible as item (item.href)}
          <NavLink {item} {isActive} />
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
        <NavLink
          item={{ href: "/account", label: account_menu(), icon: User }}
          {isActive}
        />
      </Navigation.Menu>
    </Navigation>
  {/if}

  <!-- The overflow sheet. Portaled to body; the sheet body is wrapped in
       its own stateless Navigation (bar layout) because NavLink's
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
                  {#if section.title}
                    <!-- A sheet copy of one sidebar section: optional title
                         row with its manage gear, then the overflow items.
                         Gear rows are enlarged to a 40px touch target (the
                         sidebar's size-5 is mouse-scale). -->
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
                          class="size-10 grid place-items-center rounded-base opacity-50 hover:opacity-100 hover:preset-filled-surface-500 shrink-0"
                        >
                          <ManageIcon class="size-4" />
                        </a>
                      {/if}
                    </div>
                    <div
                      class="border-b border-surface-200-800"
                      role="separator"
                    ></div>
                  {/if}
                  {#each section.items as item (item.href)}
                    <NavLink {item} {isActive} />
                  {/each}
                {/each}
              </Navigation.Menu>
            </Navigation>
          </Dialog.Content>
        </Dialog.Positioner>
      </Portal>
    </Dialog>
  {/if}
</div>
