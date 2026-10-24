<script lang="ts">
  // One transcript row's bubble: the bubble, carrying its own action row under
  // it, standing on the sender's side of the column. The wrapper here is the
  // bubble's only width cap and its alignment -- nothing else -- so a bubble
  // can never widen past the transcript's 80% whatever its content, and the
  // sender alone decides the boundary. Every chat surface draws through here,
  // so the direct, room and channel shapes cannot drift apart again.
  import type { Snippet } from "svelte";

  let {
    mine,
    children,
  }: {
    /** Own message: the bubble hugs the row's end edge. */
    mine: boolean;
    /** The bubble. */
    children: Snippet;
  } = $props();
</script>

<!-- Column, not row: the bubble's action row then stacks UNDER the bubble
     instead of becoming a sibling beside it, and the bubble is aligned inside
     this wrapper the same way the wrapper is aligned in the transcript column
     -- so the sender alone decides the boundary, not the content width. The
     single percentage cap lives here and the bubble inside takes
     `max-w-full`, so a cap can never compound. -->
<div
  class="flex max-w-[80%] min-w-0 flex-col {mine ? 'items-end' : 'items-start'}"
>
  {@render children()}
</div>
