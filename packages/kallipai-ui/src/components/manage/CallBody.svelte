<script lang="ts">
  // One call's readable parts: the summary line, the arguments, and the
  // stored text laid out by shape. The shape is chosen from the data (an
  // exit status means a terminal, a hunk head means a patch, an object means
  // a tree), never from the tool's name -- the same tool answers in
  // different shapes.
  //
  // A tool with a registered view draws its own body: this component holds
  // the one dispatch, and the registry decides. Everything else keeps the
  // shape chain below it.
  //
  // The component reads nothing. The page owns the range reads and hands the
  // text down, so one place still answers "is this answer still mine".
  import type { CallPart, CallRow } from "@kallipai/kallipai-client";
  import CopyButton from "../CopyButton.svelte";
  import JsonView from "./JsonView.svelte";
  import ViewSwitch from "./ViewSwitch.svelte";
  import StoredText from "./StoredText.svelte";
  import BlockText from "./BlockText.svelte";
  import OutputStream from "./OutputStream.svelte";
  import {
    buildJsonTree,
    callShape,
    isContainer,
    pickStream,
    terminalCommand,
    terminalOutput,
    type JsonTree,
  } from "../../lib/manage/jsonView.ts";
  import { pickRegisteredView } from "./CallViews.svelte";
  import CallNarration from "./CallNarration.svelte";
  import {
    readEnvelope,
    type ParsedEnvelope,
    type ViewPick,
  } from "../../lib/manage/toolViews.ts";
  import {
    argsView,
    bodyKey,
    callDetails,
    type BodyEntry,
    wholeText,
  } from "../../lib/manage/turnView.ts";
  import { summarize } from "../../lib/manage/toolSummary.ts";
  import {
    manage_history_args_label,
    manage_history_body_empty,
    manage_history_body_loading,
    manage_history_body_more,
    manage_history_body_range,
    manage_history_body_reclaimed,
    manage_history_error_unstated,
    manage_history_failed_line,
    manage_history_pending_line,
    manage_history_pending_hint,
    manage_history_pending_no_id,
    manage_history_refresh,
    manage_history_schema_face,
    manage_history_result_label,
    manage_history_show_original,
    manage_history_streams_pending,
    manage_history_streams_unknown,
    manage_history_text_as_kept,
    manage_history_text_too_large,
    manage_history_text_unsafe_number,
    manage_history_view_switch,
    manage_history_fallback_shown,
  } from "../../paraglide/messages.js";

  let {
    call,
    turnId,
    bodies,
    onRead,
    onMore,
  }: {
    call: CallRow;
    /** Every stored text held for this turn, keyed by `bodyKey`: the result,
     * the arguments and the error are slots in one map, and this component
     * asks for the ones it draws. */
    bodies: Readonly<Record<string, BodyEntry>>;
    /** The turn this call belongs to: a block of its text is keyed by this,
     * so the same index in another turn is a new block, collapsed. */
    turnId: number;
    /** Ask for one part's first range. */
    onRead: (part: CallPart, offset: number) => void;
    /** Ask for one part's next range, appending to what is shown. */
    onMore: (part: CallPart, offset: number) => void;
  } = $props();

  /** The call's own stored text; undefined until the reader asks for it. */
  const body = $derived(bodies[bodyKey(turnId, call.call_index, "result")]);
  /** The arguments' stored text, read on demand. */
  const argsBody = $derived(bodies[bodyKey(turnId, call.call_index, "args")]);

  let resultRaw = $state(false);
  /** Whether the arguments are shown as stored text instead of the tree. */
  let argsRaw = $state(false);
  let expanded = $state<Record<string, boolean>>({});
  let unfolded = $state<Record<string, boolean>>({});

  const facts = $derived(callDetails(call));
  const text = $derived(body?.text ?? "");
  const shape = $derived(
    callShape(
      {
        exitCode: facts.exit,
        stdoutBytes: facts.stdout?.bytes ?? null,
        stderrBytes: facts.stderr?.bytes ?? null,
      },
      text,
    ),
  );
  const args = $derived(argsView(call, "detail"));
  /** Parsed only when the arguments arrived whole: a cut is not malformed
   * JSON, and reporting it as such would be wrong twice. */
  const argsTree = $derived(
    args.complete
      ? buildJsonTree(call.args)
      : ({ ok: false, reason: "not-json" } as JsonTree),
  );
  /** Whether the arguments have a second reading for the reader to pick.
   * Parsed is not enough: a scalar reads the same as its stored text, so
   * the tree differs only once it is a container. */
  const hasArgsStructured = $derived(argsTree.ok && isContainer(argsTree.node));
  const summary = $derived(
    summarize(call.name, argsTree.ok ? argsTree.node : null),
  );
  /** Parsed only when the reader has the whole text: a half-read range is not
   * JSON, and saying so about it would be wrong twice (it is a cut, not a
   * record that failed to parse). */
  const envelope: JsonTree = $derived(
    body !== undefined && body.atEnd && text !== ""
      ? buildJsonTree(text)
      : { ok: false, reason: "empty" },
  );
  /** The result, read once per text: the registry sees this value and nothing
   * else, so a long record is parsed once, not once per render. */
  const parsed: ParsedEnvelope = $derived(readEnvelope(envelope));
  /** The one dispatch: the picker hears the name and the parsed envelope,
   * and answers with the view, the pending line, the failure line or the
   * shape chain. */
  const pick: ViewPick = $derived(pickRegisteredView(call.name, parsed));
  const out = $derived(envelope.ok ? terminalOutput(envelope.node) : null);
  /** The command this terminal block names, from the one place that knows
   * where a command lives. A made-up stand-in (the tool name and an index)
   * would read as a command the model never sent. */
  const command = $derived(terminalCommand(argsTree.ok ? argsTree.node : null));

  function toggle(path: string, next: boolean) {
    expanded = { ...expanded, [path]: next };
  }

  function unfold(path: string, next: boolean) {
    unfolded = { ...unfolded, [path]: next };
  }

  /** The id of a half's title row. The switch in that row is named by this
   * id, so the template and the switch cannot drift apart. */
  function titleId(index: number, half: "args" | "result"): string {
    return `${half}-title-${turnId}-${index}`;
  }

  /** The id of the one hidden span that says what the group is, placed after
   * the half's title in the name. */
  function switchId(index: number, half: "args" | "result"): string {
    return `${titleId(index, half)}-switch`;
  }

  /** The ids a half's switch takes its name from, in the order a reader
   * hears them: that half's title, then the word for the control. Named by
   * the title alone, the group's name would say which half it belongs to and
   * nothing about what it is. */
  function switchName(index: number, half: "args" | "result"): string {
    return `${titleId(index, half)} ${switchId(index, half)}`;
  }

  /** The one stream a single-block terminal draws. Null when the record kept
   * none, which is a shape this build cannot lay out as a terminal block. */
  const stream = $derived(out === null ? null : pickStream(out));
  /** Whether the block has a second view to switch to. Plain text and
   * patches read the same in both, so a switch there would be an empty
   * control that only changes its own label. Parsing alone is not enough
   * either: a scalar tree reads the same as its text. So the second
   * reading exists once the tree is a container: an empty one still says
   * how many entries it holds, which its text does not. The result half needs
   * nothing more: a terminal draws its streams from a payload object, and a
   * payload is here only when the whole text is, which is what envelope.ok
   * says. */
  const hasStructured = $derived(envelope.ok && isContainer(envelope.node));
  /** Whether the whole stored text is here. A record-level claim ("this
   * result kept no stream") may only be made once it is: a half-read range
   * carries no stream either, and saying so would describe the read, not the
   * record. */
  const atEnd = $derived(body !== undefined && body.atEnd);
</script>

{#each summary as item (item.field)}
  <p class="break-words font-mono text-xs">
    <span class="opacity-60">{item.field}:</span>
    {item.value}
  </p>
{/each}

<div>
  <div class="flex flex-wrap items-center gap-1">
    <span
      id={titleId(call.call_index, "args")}
      class="text-xs font-semibold text-surface-800-200"
      >{manage_history_args_label()}</span
    >
    {#if !args.more}
      <CopyButton getText={() => call.args} />
    {:else if argsBody !== undefined && argsBody.atEnd}
      {@const text = wholeText(args.text, argsBody)}
      <CopyButton getText={() => text} />
    {/if}
    {#if hasArgsStructured}
      <span class="sr-only" id={switchId(call.call_index, "args")}
        >{manage_history_view_switch()}</span
      >
      <ViewSwitch
        labelledby={switchName(call.call_index, "args")}
        value={argsRaw ? "raw" : "structured"}
        onPick={(next) => (argsRaw = next === "raw")}
      />
    {/if}
  </div>
  {#if argsTree.ok && !argsRaw}
    <JsonView
      node={argsTree.node}
      label=""
      path="$args"
      {expanded}
      {unfolded}
      onToggle={toggle}
      onUnfold={unfold}
    />
  {:else if args.more}
    {#key bodyKey(turnId, call.call_index, "args")}
      <StoredText
        text={args.text}
        more
        body={argsBody}
        onRead={(offset) => onRead("args", offset)}
        onMore={(offset) => onMore("args", offset)}
      />
    {/key}
  {:else}
    <BlockText text={call.args} />
  {/if}
</div>

<div
  class="flex flex-wrap items-center gap-1 border-t border-surface-500-400 pt-2"
>
  <span
    id={titleId(call.call_index, "result")}
    class="text-xs font-semibold text-surface-800-200"
    >{manage_history_result_label()}</span
  >
  {#if text !== "" && atEnd}
    <CopyButton getText={() => body?.text ?? ""} />
  {/if}
  {#if hasStructured}
    <span class="sr-only" id={switchId(call.call_index, "result")}
      >{manage_history_view_switch()}</span
    >
    <ViewSwitch
      labelledby={switchName(call.call_index, "result")}
      value={resultRaw ? "raw" : "structured"}
      onPick={(next) => (resultRaw = next === "raw")}
    />
  {/if}
</div>

{#if body === undefined}
  <button
    type="button"
    class="text-xs text-surface-800-200 transition hover:opacity-70"
    onclick={() => onRead("result", 0)}
  >
    {manage_history_show_original()}
  </button>
{:else}
  {#if body.loading}
    <p class="text-xs opacity-60">{manage_history_body_loading()}</p>
  {:else if body.error}
    <div class="flex flex-wrap items-center gap-2 text-xs">
      <p class="text-error-900-100">{body.error}</p>
      <button
        type="button"
        class="btn btn-sm preset-outlined-surface-500 text-xs hover:preset-filled-surface-500"
        onclick={() => onRead("result", 0)}>{manage_history_refresh()}</button
      >
    </div>
  {:else if body.unsupported !== null}
    <p class="text-xs text-surface-700-300">
      {manage_history_schema_face()}
    </p>
  {:else if body.empty}
    <p class="text-xs opacity-60">{manage_history_body_empty()}</p>
  {:else}
    {#if body.more !== null}
      <p class="text-xs opacity-60">
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

    <div class:hidden={resultRaw}>
      {#if pick.kind === "pending"}
        <CallNarration
          tone="info"
          mono={pick.pending.id !== null}
          text={pick.pending.id === null
            ? manage_history_pending_no_id()
            : manage_history_pending_line({ id: pick.pending.id })}
        />
        {#if pick.pending.id !== null}
          <p class="pl-3.5 text-xs text-surface-800-200">
            {manage_history_pending_hint()}
          </p>
        {/if}
      {:else if pick.kind === "dedicated"}
        {@const View = pick.entry.component}
        <View
          name={call.name}
          args={argsTree.ok ? argsTree.node : null}
          node={pick.node}
          payload={pick.payload}
        />
      {:else if pick.kind === "failed"}
        <CallNarration
          tone="error"
          text={manage_history_failed_line({
            reason: pick.error ?? manage_history_error_unstated(),
          })}
        />
      {:else}
        {#if pick.registered}
          <p class="text-xs text-surface-700-300">
            {manage_history_fallback_shown()}
          </p>
        {/if}
        {#if shape === "terminal"}
          <div class="space-y-1">
            {#if command !== null}
              <p class="break-words font-mono text-xs">
                <span class="opacity-60">$</span>
                {command}
              </p>
            {/if}
            {#if out !== null && stream !== null}
              <OutputStream output={out} />
            {:else if atEnd}
              <p class="text-xs opacity-60">
                {envelope.ok
                  ? manage_history_streams_unknown()
                  : manage_history_text_as_kept()}
              </p>
              <BlockText {text} />
            {:else}
              <p class="text-xs opacity-60">
                {manage_history_streams_pending()}
              </p>
            {/if}
          </div>
        {:else if envelope.ok}
          <JsonView
            node={envelope.node}
            label=""
            path="$text"
            {expanded}
            {unfolded}
            onToggle={toggle}
            onUnfold={unfold}
          />
        {:else}
          {#if envelope.reason === "unsafe-number" || envelope.reason === "too-large"}
            <p class="text-xs opacity-60">
              {envelope.reason === "unsafe-number"
                ? manage_history_text_unsafe_number()
                : manage_history_text_too_large()}
            </p>
          {/if}
          <BlockText {text} />
        {/if}
      {/if}
    </div>

    <div class:hidden={!resultRaw}>
      <BlockText text={body.text} />
    </div>

    {#if !body.atEnd && body.more !== null}
      <button
        type="button"
        class="btn btn-sm preset-outlined-surface-500 text-xs hover:preset-filled-surface-500"
        onclick={() => onMore("result", body?.more ?? 0)}
      >
        {manage_history_body_more()}
      </button>
    {:else if !body.atEnd}
      <button
        type="button"
        class="btn btn-sm preset-outlined-surface-500 text-xs hover:preset-filled-surface-500"
        onclick={() => onRead("result", 0)}>{manage_history_refresh()}</button
      >
    {/if}
  {/if}
{/if}
