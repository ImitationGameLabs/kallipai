<script lang="ts">
  // One stored text: a box that scrolls, and, where the face kept only a
  // prefix, one control that reads on from the byte the prefix ends at. The
  // page owns every read; this block owns what it asks for and what it draws
  // while the answer is on its way.
  import BlockText from "./BlockText.svelte";
  import {
    type BodyEntry,
    prefixBytes,
    wholeText,
  } from "../../lib/manage/turnView.ts";
  import {
    manage_history_body_empty,
    manage_history_body_loading,
    manage_history_body_more,
    manage_history_body_range,
    manage_history_body_reclaimed,
    manage_history_body_stopped,
    manage_history_refresh,
    manage_history_schema_face,
  } from "../../paraglide/messages.js";

  let {
    text,
    more,
    body,
    onRead,
    onMore,
    kind = "machine",
    failed = false,
  }: {
    /** The text the face sent; null when it sent none. */
    text: string | null;
    /** Whether the text is a prefix whose rest is a body read. */
    more: boolean;
    /** Undefined until the reader asks for the rest. */
    body: BodyEntry | undefined;
    /** Ask for the rest, from the byte this text ends at. */
    onRead: (offset: number) => void;
    /** Ask for the next range, appending to what is shown. */
    onMore: (offset: number) => void;
    /** Who wrote the text: a tool's output, or a message meant for a person. */
    kind?: "machine" | "prose";
    /** The text is a failure, so it reads in the error tone. */
    failed?: boolean;
  } = $props();

  /** Where the rest of this text starts: the byte length of what is drawn.
   * Asking from zero would hand back the prefix already on screen. */
  const restFrom = $derived(prefixBytes(text));

  function readRest(): void {
    onRead(restFrom);
  }

  function readMore(): void {
    const next = body?.more ?? null;
    if (next !== null) onMore(next);
  }
</script>

{#if more && body !== undefined}
  {#if body.loading}
    {#if text !== null}<BlockText {text} {failed} {kind} />{/if}
    <p class="text-xs opacity-60">{manage_history_body_loading()}</p>
  {:else if body.error !== null}
    {#if text !== null}<BlockText {text} {failed} {kind} />{/if}
    <p class="text-xs text-error-900-100">{body.error}</p>
    <button
      type="button"
      class="btn btn-sm preset-outlined-surface-500 text-xs hover:preset-filled-surface-500"
      onclick={readRest}>{manage_history_refresh()}</button
    >
  {:else if body.unsupported !== null}
    {#if text !== null}<BlockText {text} {failed} {kind} />{/if}
    <p class="text-xs text-surface-700-300">
      {manage_history_schema_face()}
    </p>
  {:else if body.empty}
    {#if text !== null}<BlockText {text} {failed} {kind} />{/if}
    <p class="text-xs opacity-60">{manage_history_body_empty()}</p>
  {:else}
    <BlockText text={wholeText(text, body)} {failed} {kind} />
    {#if body.more !== null}
      <p class="text-xs text-surface-700-300">
        {manage_history_body_range({
          from: body.from.toLocaleString(),
          to: body.to.toLocaleString(),
          total: body.total.toLocaleString(),
        })}
      </p>
    {/if}
    {#if body.reclaimed > 0}
      <p class="text-xs opacity-60">
        {manage_history_body_reclaimed({ count: body.reclaimed })}
      </p>
    {/if}
    {#if body.more !== null}
      <button
        type="button"
        class="btn btn-sm preset-outlined-surface-500 text-xs hover:preset-filled-surface-500"
        onclick={readMore}>{manage_history_body_more()}</button
      >
    {:else if !body.atEnd}
      <p class="text-xs opacity-60">{manage_history_body_stopped()}</p>
      <button
        type="button"
        class="btn btn-sm preset-outlined-surface-500 text-xs hover:preset-filled-surface-500"
        onclick={readRest}>{manage_history_refresh()}</button
      >
    {/if}
  {/if}
{:else}
  {#if text !== null}
    <BlockText {text} {failed} {kind} />
  {/if}
  {#if more}
    <button
      type="button"
      class="btn btn-sm preset-outlined-surface-500 text-xs hover:preset-filled-surface-500"
      onclick={readRest}>{manage_history_body_more()}</button
    >
  {/if}
{/if}
