<script lang="ts">
  // The shared kebab actions menu (the EnrollmentCodeCard pattern): a
  // vertical-dots trigger opening a Skeleton Menu, so card and row bodies
  // stay identity-only with the actions one popover away. The items are
  // passed as the snippet content (Menu.Item keeps its value and the
  // parent's onSelect); item classes, separators, and the destructive
  // styling stay with the caller, because they are per-action semantics.
  import { Menu, Portal } from "@skeletonlabs/skeleton-svelte";
  import { MoreVertical } from "@lucide/svelte";
  import { TONAL_ICON_SURF } from "../lib/classes.ts";
  import type { Snippet } from "svelte";

  let {
    label,
    placement = "bottom-end",
    disabled = false,
    compact = false,
    contentClass = "card preset-tonal-surface p-1 min-w-[8rem]",
    onSelect,
    children,
  }: {
    /** The trigger's accessible name (i18n at the call site). */
    label: string;
    /** The popover side: bottom for in-flow rows, top for card corners. */
    placement?: "bottom-end" | "top-end";
    /** Disables the trigger (the caller's busy/read-only state). */
    disabled?: boolean;
    /** The size-8 trigger for dense cards; the default is the section's size-10. */
    compact?: boolean;
    /** The content's classes; widen for menus with long item labels. */
    contentClass?: string;
    onSelect: (value: string) => void;
    children: Snippet;
  } = $props();
</script>

<Menu positioning={{ placement }} onSelect={(e) => onSelect(e.value)}>
  <Menu.Trigger
    class="{compact ? 'size-8' : 'size-10 shrink-0'} {TONAL_ICON_SURF}"
    aria-label={label}
    {disabled}
  >
    <MoreVertical class="size-4" />
  </Menu.Trigger>
  <Portal>
    <Menu.Positioner>
      <Menu.Content class={contentClass}>
        {@render children()}
      </Menu.Content>
    </Menu.Positioner>
  </Portal>
</Menu>
