<script lang="ts">
  // One tool call: the facts a scan needs, then the body (summary, arguments,
  // stored text) handed to its own component. This file decides nothing about
  // how an answer is laid out -- CallBody does, in one place, so a dedicated
  // view for a tool can replace that one place and nothing else.
  import type { CallPart, CallRow } from "@kallipai/kallipai-client";
  import CallBody from "./CallBody.svelte";
  import StoredText from "./StoredText.svelte";
  import type { BodyEntry } from "../../lib/manage/turnView.ts";
  import {
    bodyKey,
    callDetails,
    callStatus,
  } from "../../lib/manage/turnView.ts";
  import {
    manage_turns_call_cut,
    manage_turns_call_exit,
    manage_turns_call_failed,
    manage_turns_call_ok,
    manage_turns_call_pending,
    manage_turns_call_timed_out,
    manage_turns_stderr_label,
    manage_turns_stdout_label,
    manage_turns_stream_byte_one,
    manage_turns_stream_byte_other,
    manage_turns_stream_line_one,
    manage_turns_stream_line_other,
    manage_turns_stream_shape,
  } from "../../paraglide/messages.js";

  let {
    call,
    turnId,
    bodies,
    onRead,
    onMore,
  }: {
    call: CallRow;
    /** The turn this call belongs to: a block of its text is keyed by this,
     * so the same index in another turn is a new block, collapsed. */
    turnId: number;
    /** Every stored text held for this turn, keyed by `bodyKey`. One map, so
     * a caller never builds a second spelling of the same slot. */
    bodies: Readonly<Record<string, BodyEntry>>;
    /** Ask for one part's first range. */
    onRead: (part: CallPart, offset: number) => void;
    /** Ask for one part's next range, appending to what is shown. */
    onMore: (part: CallPart, offset: number) => void;
  } = $props();

  const status = $derived(callStatus(call));
  const facts = $derived(callDetails(call));
  const errorBody = $derived(bodies[bodyKey(turnId, call.call_index, "error")]);
  const statusText = $derived(
    status === "ok"
      ? manage_turns_call_ok()
      : status === "pending"
        ? manage_turns_call_pending()
        : manage_turns_call_failed(),
  );
  const exitClass = $derived(
    facts.exit === 0 ? "text-success-950-50" : "text-error-900-100",
  );

  /** One stream's size, each count agreeing with its noun, so a stream of one
   * byte or one line does not read as a plural. */
  function streamLine(
    label: string,
    size: { bytes: number; lines: number },
  ): string {
    const bytes = size.bytes.toLocaleString();
    const lines = size.lines.toLocaleString();
    return manage_turns_stream_shape({
      label,
      bytes:
        size.bytes === 1
          ? manage_turns_stream_byte_one({ bytes })
          : manage_turns_stream_byte_other({ bytes }),
      lines:
        size.lines === 1
          ? manage_turns_stream_line_one({ lines })
          : manage_turns_stream_line_other({ lines }),
    });
  }
</script>

<div class="space-y-1 rounded bg-surface-100-900 p-2">
  <div class="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs">
    <span class="font-mono opacity-60">#{call.call_index}</span>
    <span class="font-mono font-medium">{call.name}</span>
    <span
      class={status === "failed"
        ? "text-error-900-100"
        : status === "pending"
          ? "text-warning-950-50"
          : "text-success-950-50"}>{statusText}</span
    >
    {#if facts.exit !== null}
      <span class="font-mono {exitClass}"
        >{manage_turns_call_exit({ code: facts.exit })}</span
      >
    {/if}
    {#if facts.timedOut}
      <span class="text-warning-950-50">{manage_turns_call_timed_out()}</span>
    {/if}
    {#if facts.cut}
      <span class="opacity-60">{manage_turns_call_cut()}</span>
    {/if}
  </div>
  {#if facts.firstError}
    {@const first = facts.firstError}
    {#if first.more}
      {#key bodyKey(turnId, call.call_index, "error")}
        <StoredText
          text={first.text}
          more
          body={errorBody}
          onRead={(offset) => onRead("error", offset)}
          onMore={(offset) => onMore("error", offset)}
          failed
        />
      {/key}
    {:else}
      <p class="break-words font-mono text-xs text-error-900-100">
        {first.text}
      </p>
    {/if}
  {/if}

  <!-- These tallies are the call row's own stream counts, while the
  stored text block below is a range read of the same text: the two
  numbers answer different questions and never sum to each other. -->
  {#if facts.stdout || facts.stderr}
    <p class="text-xs text-surface-800-200">
      {#if facts.stdout}{streamLine(
          manage_turns_stdout_label(),
          facts.stdout,
        )}{/if}
      {#if facts.stderr}· {streamLine(
          manage_turns_stderr_label(),
          facts.stderr,
        )}{/if}
    </p>
  {/if}

  <CallBody {call} {turnId} {bodies} {onRead} {onMore} />
</div>
