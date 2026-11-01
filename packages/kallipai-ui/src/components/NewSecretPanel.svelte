<script lang="ts">
  // The one-time secret panel: the plaintext is shown once, copied on request,
  // then dismissed. `hint` carries whatever the caller has to say about when
  // the secret takes effect.
  import { copyText } from "../lib/clipboard.ts";
  import { common_cancel, common_copied } from "../paraglide/messages.js";

  let {
    title,
    secret,
    copyLabel,
    description = null,
    hint = null,
    onDismiss = null,
  }: {
    title: string;
    secret: string;
    copyLabel: string;
    description?: string | null;
    hint?: string | null;
    onDismiss?: (() => void) | null;
  } = $props();

  let copied = $state(false);

  async function copy(): Promise<void> {
    copied = await copyText(secret);
  }
</script>

<div
  class="space-y-2 rounded border border-warning-500/50 p-3 text-sm"
  role="status"
>
  <p class="font-medium">{title}</p>
  {#if description}
    <p class="text-xs opacity-70">{description}</p>
  {/if}
  <code class="block text-xs font-mono break-all">{secret}</code>
  {#if hint}
    <p class="text-xs opacity-70">{hint}</p>
  {/if}
  <div class="flex flex-wrap items-center gap-2">
    <button
      type="button"
      class="btn btn-sm preset-filled-primary-500"
      onclick={copy}>{copyLabel}</button
    >
    {#if copied}
      <span class="text-xs opacity-70">{common_copied()}</span>
    {/if}
    {#if onDismiss}
      <button
        type="button"
        class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
        onclick={onDismiss}>{common_cancel()}</button
      >
    {/if}
  </div>
</div>
