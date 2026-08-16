<script lang="ts">
  // Self-contained copy affordance for a single message. Reads the source text
  // fresh via `getText` on each click (assistant markdown source, not the
  // rendered HTML) and flashes a check mark for ~1.5s on success.
  //
  // Unlike EnrollmentCodeCard (whose copied highlight is parent-owned because
  // the store owns the clipboard write), this owns its own transient feedback:
  // it is a leaf with no parent interest in copy state, and <Markdown> mounts
  // once per finalized message so nothing outlives a reuse.
  import { Copy, Check } from "@lucide/svelte";
  import { copyText } from "../lib/clipboard.ts";
  import { common_copy, common_copied } from "../paraglide/messages.js";

  let {
    getText,
    class: klass = "",
  }: {
    getText: () => string;
    class?: string;
  } = $props();

  let copied = $state(false);
  let timer: ReturnType<typeof setTimeout> | undefined;

  async function onclick(): Promise<void> {
    if (await copyText(getText())) {
      copied = true;
      clearTimeout(timer);
      timer = setTimeout(() => {
        copied = false;
      }, 1500);
    }
  }
</script>

<button
  type="button"
  {onclick}
  aria-label={copied ? common_copied() : common_copy()}
  class="rounded p-1.5 text-surface-500 dark:text-surface-400 opacity-60 [@media(hover:hover)]:opacity-0 [@media(hover:hover)]:group-hover:opacity-100 focus-visible:opacity-100 hover:bg-surface-200-800 transition {klass}"
>
  {#if copied}
    <Check class="size-4" />
  {:else}
    <Copy class="size-4" />
  {/if}
</button>
