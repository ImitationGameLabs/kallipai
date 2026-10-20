<script lang="ts">
  // The quote affordance a transcript places beside a bubble: the direct quote
  // button and the overflow menu carrying the same action, in one wrapper both
  // surfaces share. Only the side differs between them, and only the row that
  // draws the bubble knows it. The open flag and the action stay the parent's,
  // because one menu can be open per transcript.
  import MessageActions from "./MessageActions.svelte";

  let {
    side,
    menuOpen,
    onMenuOpenChange,
    onQuote,
  }: {
    /** Which side of the row the bubble sits on: the controls hang off the
     *  opposite side. */
    side: "left" | "right";
    /** Whether this row's menu is the transcript's open one. */
    menuOpen: boolean;
    onMenuOpenChange: (open: boolean) => void;
    onQuote: () => void;
  } = $props();

  // The two sides differ by one anchoring class, and `side` is a prop, so the
  // class is picked here once instead of at every call site.
  const anchored = $derived(
    side === "right" ? "right-full mr-1" : "left-full ml-1",
  );
</script>

<!-- Beside the bubble, not under it: absolutely placed so the controls cost no
     layout, and hover-revealed by the row's own group. Touch reaches the same
     menu by long press. -->
<div class="absolute top-1/2 -translate-y-1/2 {anchored}">
  <MessageActions {menuOpen} {onMenuOpenChange} {onQuote} />
</div>
