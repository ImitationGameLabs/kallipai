<script lang="ts">
  // One block's two readings, side by side: the shape the page draws from the
  // text (a tree, a stream, a registered view) and the text as stored. A
  // segmented control, not two links or a pair of buttons: one of the two is
  // always on show, and the reader sees both before choosing. The arguments
  // half and the result half use this one component, so the two blocks cannot
  // drift into two different controls.
  // The control brings its own 8px padding and gap (skeleton-common
  // segmented-control.css); p-0 and gap-1 press them to 0 and 4px, so the
  // bar stands 33px tall instead of 49.
  import { SegmentedControl } from "@skeletonlabs/skeleton-svelte";

  import {
    manage_turns_raw,
    manage_turns_structured,
  } from "../../paraglide/messages.js";

  let {
    labelledby,
    value,
    onPick,
  }: {
    /** The ids naming this control, space separated and in the order a
     * reader hears them. */
    labelledby: string;
    /** Which reading is on show. */
    value: "structured" | "raw";
    /** The reading the reader picked. */
    onPick: (next: "structured" | "raw") => void;
  } = $props();

  function onValueChange(details: { value: string | null }) {
    const next = details.value;
    if (next === "structured" || next === "raw") onPick(next);
  }
</script>

<SegmentedControl {value} {onValueChange} aria-labelledby={labelledby}>
  <SegmentedControl.Control class="items-center gap-1 p-0">
    <SegmentedControl.Item
      value="structured"
      class="btn btn-sm flex-none text-xs hover:bg-surface-200-800 data-[state=checked]:preset-tonal-primary data-[state=checked]:font-semibold data-[state=checked]:ring-1 data-[state=checked]:ring-primary-800 dark:data-[state=checked]:ring-primary-500"
    >
      <SegmentedControl.ItemHiddenInput />
      <SegmentedControl.ItemText
        class="text-surface-950-50! data-[state=checked]:text-primary-950-50!"
        >{manage_turns_structured()}</SegmentedControl.ItemText
      >
    </SegmentedControl.Item>
    <SegmentedControl.Item
      value="raw"
      class="btn btn-sm flex-none text-xs hover:bg-surface-200-800 data-[state=checked]:preset-tonal-primary data-[state=checked]:font-semibold data-[state=checked]:ring-1 data-[state=checked]:ring-primary-800 dark:data-[state=checked]:ring-primary-500"
    >
      <SegmentedControl.ItemHiddenInput />
      <SegmentedControl.ItemText
        class="text-surface-950-50! data-[state=checked]:text-primary-950-50!"
        >{manage_turns_raw()}</SegmentedControl.ItemText
      >
    </SegmentedControl.Item>
  </SegmentedControl.Control>
</SegmentedControl>
