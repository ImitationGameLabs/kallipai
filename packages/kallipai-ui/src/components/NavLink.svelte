<script lang="ts">
  import { Navigation } from "@skeletonlabs/skeleton-svelte";
  import {
    navIndicatorDotClass,
    navIndicatorLabel,
    type NavItem,
  } from "../lib/shell.ts";
  import { badgeLabel } from "../lib/session/unread.svelte.ts";
  import { chat_unread_badge_aria } from "../paraglide/messages.js";

  let {
    item,
    isActive,
    compact = false,
  }: {
    item: NavItem;
    // The consumer-supplied route matcher ("/" exact, others by prefix),
    // relayed from the shell: active styling is a routing question, not
    // a NavLink decision.
    isActive: (href: string) => boolean;
    // The small-screen bar shows a cell one word wide, so those rows drop
    // the stamp and keep the badge. A prop and not a breakpoint: the
    // overflow sheet renders these same rows and does show the stamp.
    compact?: boolean;
  } = $props();

  const active = $derived(isActive(item.href));
</script>

<!-- The gap between a name and its trailing group belongs to the surface,
     not to this component: the bar keeps the base spacing, and the
     sidebar adds the room an ellipsis needs before the stamp
     (DesktopShell). The name still yields here, so it only shortens
     when it does not fit. -->
<Navigation.TriggerAnchor
  href={item.href}
  aria-current={active ? "page" : undefined}
  class={active
    ? "preset-filled-surface-500"
    : "preset-tonal-surface hover:preset-filled-surface-500"}
>
  {#if item.indicator === "pending"}
    <!-- A spinning ring (not a filled dot): a filled dot has no visible
         rotation axis, so the border + transparent top segment reads as
         motion. Size-matched to the size-2 status dot. aria-hidden; the
         sr-only "connecting" label below carries state. -->
    <span
      class="size-2 rounded-full border-2 border-surface-400-600 border-t-transparent animate-spin shrink-0"
      aria-hidden="true"
    ></span>
  {:else if item.indicator}
    <span
      class="size-2 rounded-full shrink-0 {navIndicatorDotClass(
        item.indicator,
      )}"
      aria-hidden="true"
    ></span>
  {:else if item.icon}
    {@const Icon = item.icon}
    <Icon class="size-4" />
  {/if}
  <!-- The name yields to the trailing group: without min-w-0 a flex item
       keeps its min-content width, so a long name would spill and clip the
       stamp instead of ellipsizing. -->
  <Navigation.TriggerText class="min-w-0 truncate"
    >{item.label}</Navigation.TriggerText
  >
  {#if item.indicator}
    <span class="sr-only">{navIndicatorLabel(item.indicator)}</span>
  {/if}
  {#if item.time !== undefined || (item.badge ?? 0) > 0}
    <!-- Everything at the row's trailing edge sits in one group under one
         auto margin: two auto margins would split the spare space and leave
         the stamp short of the pill. -->
    <span class="ml-auto flex items-center gap-2 shrink-0">
      {#if item.time !== undefined && !compact}
        <!-- When this conversation last moved. -->
        <!-- 80% opacity clears 4.5:1 in both modes. -->
        <span class="text-xs tabular-nums opacity-80">{item.time}</span>
      {/if}
      {#if (item.badge ?? 0) > 0}
        <!-- The unread pill: the count (capped at 99+ by badgeLabel). One
             render point covers sidebar, bar and sheet -- every nav cell
             across the shells is this component. -->
        <span
          class="rounded-full px-1.5 py-0.5 text-[11px] leading-none font-semibold preset-filled-primary-500"
        >
          {badgeLabel(item.badge ?? 0)}
        </span>
        <span class="sr-only">
          {chat_unread_badge_aria({ count: badgeLabel(item.badge ?? 0) })}
        </span>
      {/if}
    </span>
  {/if}
</Navigation.TriggerAnchor>
