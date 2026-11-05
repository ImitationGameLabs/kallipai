<script lang="ts">
  // A terminal record's output, in one implementation. The record decides the
  // layout: a capture that kept stdout and stderr apart draws two labelled
  // blocks, anything else draws the one stream it kept. No caller passes a
  // mode: one stream and two streams are two layouts, not one block with a
  // switch on it.
  //
  // A separated capture marks each stream in its label, and the labels are the
  // only place that identity is stated: the stderr block does not also carry an
  // edge, so one stream is not coloured three times over. The merged block's
  // edge comes from the payload's exit code, read here rather than handed in.
  //
  // Nothing here is half-read: the streams are read from a payload, and a
  // payload exists only once the whole stored text is in.
  import {
    pickStream,
    type TerminalOutput,
  } from "../../lib/manage/jsonView.ts";
  import {
    manage_history_stderr_label,
    manage_history_stdout_label,
  } from "../../paraglide/messages.js";
  import BlockText from "./BlockText.svelte";

  let {
    output,
  }: {
    /** The streams this record kept, already read off its payload. */
    output: TerminalOutput;
  } = $props();

  const split = $derived(
    output.stdout !== null && output.stderr !== null
      ? { out: output.stdout, err: output.stderr }
      : null,
  );
  const merged = $derived(pickStream(output));
  const failed = $derived(output.exitCode !== null && output.exitCode !== 0);
</script>

{#if split !== null}
  <div class="space-y-1">
    <p
      class="inline-block rounded-sm ring-1 ring-surface-200-800 bg-surface-50-950 px-1.5 py-0.5 text-xs font-semibold tracking-wide text-surface-700-300 uppercase"
    >
      {manage_history_stdout_label()}
    </p>
    <BlockText text={split.out} />
    <p
      class="mt-2 inline-block rounded-sm ring-1 ring-error-300-700 bg-error-50-950 px-1.5 py-0.5 text-xs font-bold tracking-wide text-error-600-400 uppercase"
    >
      {manage_history_stderr_label()}
    </p>
    <BlockText text={split.err} />
  </div>
{:else if merged !== null}
  <BlockText text={merged} {failed} />
{/if}
