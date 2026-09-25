<script lang="ts">
  // A tiny circular "!" explaining its neighbor on hover. Native title
  // tooltips are pointer-only; touch screens get no hover.
  let { text }: { text: string } = $props();
</script>

<span
  class="inline-flex size-4 shrink-0 items-center justify-center rounded-full border border-current text-[10px] font-bold leading-none opacity-80 cursor-help"
  title={text}
  aria-label={text}
>
  !
</span>
