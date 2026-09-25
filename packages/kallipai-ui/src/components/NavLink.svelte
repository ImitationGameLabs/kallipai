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
  }: {
    item: NavItem;
    // The consumer-supplied route matcher ("/" exact, others by prefix),
    // relayed from the shell: active styling is a routing question, not
    // a NavLink decision.
    isActive: (href: string) => boolean;
  } = $props();

  const active = $derived(isActive(item.href));
</script>

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
  <Navigation.TriggerText>{item.label}</Navigation.TriggerText>
  {#if item.indicator}
    <span class="sr-only">{navIndicatorLabel(item.indicator)}</span>
  {/if}
  {#if (item.badge ?? 0) > 0}
    <!-- The unread pill: the count (capped at 99+ by badgeLabel). One
         render point covers sidebar, bar and sheet -- every nav cell
         across the shells is this component. -->
    <span
      class="ml-auto shrink-0 rounded-full px-1.5 py-0.5 text-[11px] leading-none font-semibold preset-filled-primary-500"
    >
      {badgeLabel(item.badge ?? 0)}
    </span>
    <span class="sr-only">
      {chat_unread_badge_aria({ count: badgeLabel(item.badge ?? 0) })}
    </span>
  {/if}
</Navigation.TriggerAnchor>
