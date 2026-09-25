<script lang="ts">
  import { TriangleAlert } from "@lucide/svelte";

  // Form-level submit errors render inline below the inputs and above the
  // submit button; field-level validation stays next to its field. All text
  // is caller-supplied (already localized) -- this component adds none.
  let {
    message,
    detail = undefined,
    hint = undefined,
  }: {
    message: string | null | undefined;
    detail?: string;
    hint?: string;
  } = $props();
</script>

{#if message}
  <div
    role="alert"
    class="space-y-1 text-sm text-error-500 dark:text-error-400"
  >
    <p class="flex items-center gap-2">
      <TriangleAlert class="size-4.5 shrink-0" aria-hidden="true" />
      {message}
    </p>
    {#if detail}<p class="opacity-80">{detail}</p>{/if}
    {#if hint}<p class="opacity-60">{hint}</p>{/if}
  </div>
{/if}
