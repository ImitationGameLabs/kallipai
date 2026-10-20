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
    triggerClass,
    contentClass = "card preset-tonal-surface p-1 min-w-[8rem]",
    open,
    onOpenChange,
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
    /** Replaces the default trigger classes outright: a caller owning its own
     *  visual (the message row's hover affordances) passes one class string
     *  instead of stacking corrections onto the shared default. */
    triggerClass?: string;
    /** The content's classes; widen for menus with long item labels. */
    contentClass?: string;
    /** Controlled open state. Give both props or neither: the transcript
     *  keeps one menu open at a time, so the flag lives with the parent. */
    open?: boolean;
    onOpenChange?: (open: boolean) => void;
    onSelect: (value: string) => void;
    children: Snippet;
  } = $props();

  // The props the menu root takes when the caller owns the open state;
  // nothing at all when it does not, so a plain row keeps the menu's own.
  const controlled = $derived.by(() => {
    const notify = onOpenChange;
    if (open === undefined || notify === undefined) return {};
    return { open, onOpenChange: (d: { open: boolean }) => notify(d.open) };
  });
</script>

<Menu
  positioning={{ placement }}
  onSelect={(e) => onSelect(e.value)}
  {...controlled}
>
  <Menu.Trigger
    class={triggerClass ??
      `${compact ? "size-8" : "size-10 shrink-0"} ${TONAL_ICON_SURF}`}
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
