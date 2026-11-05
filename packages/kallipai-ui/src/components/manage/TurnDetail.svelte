<script lang="ts">
  // One turn: what it was recorded on, its messages, and its tool calls.
  // Everything shown is copied from the record; nothing is recomputed except
  // which messages are worth drawing and which reasoning blocks are folded.
  import type {
    CallPart,
    TextPart,
    TurnDetail as TurnRecord,
  } from "@kallipai/kallipai-client";
  import CopyButton from "../CopyButton.svelte";
  import TurnCall from "./TurnCall.svelte";
  import StoredText from "./StoredText.svelte";
  import { type BodyEntry } from "../../lib/manage/turnView.ts";
  import {
    type CardError,
    bodyKey,
    carriesText,
    messageView,
    reasoningView,
    reasoningKey,
    visibleMessages,
    ranTools,
  } from "../../lib/manage/turnView.ts";
  import {
    manage_history_calls_label,
    manage_history_empty,
    manage_history_effort_value,
    manage_history_elapsed_label,
    manage_history_elapsed_value,
    manage_history_load_failed,
    manage_history_refresh,
    manage_history_loading,
    manage_history_messages_label,
    manage_history_profile_id,
    manage_history_reasoning_flag,
    manage_history_reasoning_hide,
    manage_history_reasoning_show,
    manage_history_reasoning_unavailable,
    manage_history_schema_face,
    manage_history_turn_empty,
    manage_history_turn_id,
    manage_history_usage_label,
    manage_history_usage_line,
  } from "../../paraglide/messages.js";

  let {
    detail,
    loading,
    fire,
    error,
    face,
    empty,
    bodies,
    onReadBody,
    onRetry,
    onMoreBody,
    onReadMessage,
    onMoreMessage,
  }: {
    detail: TurnRecord | null;
    loading: boolean;
    fire: CardError;
    error: string | null;
    face: number | null;
    empty: boolean;
    bodies: Readonly<Record<string, BodyEntry>>;
    onReadBody: (
      turnId: number,
      callIndex: number,
      part: CallPart,
      offset: number,
    ) => void;
    onMoreBody: (
      turnId: number,
      callIndex: number,
      part: CallPart,
      offset: number,
    ) => void;
    onReadMessage: (
      turnId: number,
      messageIndex: number,
      part: TextPart,
      role: string,
      offset: number,
    ) => void;
    onMoreMessage: (
      turnId: number,
      messageIndex: number,
      part: TextPart,
      offset: number,
      role: string,
    ) => void;
    onRetry: () => void;
  } = $props();

  /** Which reasoning blocks are open, by turn and message index. Display
   * state only, so it lives with the display. */
  let openReasoning = $state<Record<string, boolean>>({});

  function reasoningOpen(turnId: number, index: number): boolean {
    return openReasoning[reasoningKey(turnId, index)] === true;
  }

  function toggleReasoning(turnId: number, index: number) {
    const key = reasoningKey(turnId, index);
    openReasoning = { ...openReasoning, [key]: !reasoningOpen(turnId, index) };
  }
</script>

<div class="space-y-3">
  {#if loading && detail === null}
    <p class="text-xs opacity-70">{manage_history_loading()}</p>
  {:else if fire === "failed"}
    <div class="flex flex-wrap items-center gap-x-2 text-xs">
      <p class="text-error-500">{error ?? manage_history_load_failed()}</p>
      <button
        type="button"
        class="opacity-60 transition hover:opacity-100"
        onclick={onRetry}>{manage_history_refresh()}</button
      >
    </div>
  {:else if face !== null}
    <p class="text-xs text-surface-700-300">
      {manage_history_schema_face()}
    </p>
  {:else if empty}
    <p class="text-xs opacity-70">{manage_history_empty()}</p>
  {:else if detail}
    {@const turn = detail.turn}
    {@const shown = visibleMessages(turn)}
    <div class="space-y-1 rounded border border-surface-200-800 p-3">
      <div class="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs">
        <span class="font-mono font-medium"
          >{manage_history_turn_id({ id: turn.turn_id })}</span
        >
        <CopyButton getText={() => String(turn.turn_id)} />
        {#if turn.profile}
          <span class="opacity-70"
            >{turn.profile.provider} · {turn.profile.model}</span
          >
          <span class="font-mono opacity-60"
            >{manage_history_profile_id({ id: turn.profile.profile_id })}</span
          >
          {#if turn.profile.effort}
            <span class="opacity-60"
              >{manage_history_effort_value({
                effort: turn.profile.effort,
              })}</span
            >
          {/if}
        {/if}
        {#if turn.elapsed_ms !== undefined}
          <span class="opacity-60"
            >{manage_history_elapsed_label()}:
            {manage_history_elapsed_value({ ms: turn.elapsed_ms })}</span
          >
        {/if}
      </div>
      {#if turn.usage}
        <p class="text-xs opacity-60">
          {manage_history_usage_label()}:
          {manage_history_usage_line({
            prompt: turn.usage.prompt,
            completion: turn.usage.completion,
            cacheRead: turn.usage.cache_read,
            cacheWrite: turn.usage.cache_write,
            reasoning: turn.usage.reasoning,
          })}
        </p>
      {/if}
    </div>

    {#if turn.messages.length === 0 && !ranTools(turn)}
      <p class="text-xs opacity-70">{manage_history_turn_empty()}</p>
    {/if}

    {#if shown.length > 0}
      <div class="space-y-2">
        <p class="text-xs font-semibold text-surface-800-200">
          {manage_history_messages_label()}
        </p>
        {#each shown as slot (slot.index)}
          {@const view = messageView(slot.message)}
          {@const thought = reasoningView(slot.message)}
          {@const open = reasoningOpen(turn.turn_id, slot.index)}
          {@const carries = carriesText(view.text, view.more)}
          {@const showsThought = thought.state === "text" && open}
          <div class="space-y-1">
            <div
              class="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs opacity-70"
            >
              <span class="font-mono">{slot.message.role}</span>
              {#if thought.state === "text"}
                <button
                  type="button"
                  class="text-surface-800-200 transition hover:opacity-70"
                  onclick={() => toggleReasoning(turn.turn_id, slot.index)}
                >
                  {open
                    ? manage_history_reasoning_hide()
                    : manage_history_reasoning_show()}
                </button>
              {:else if thought.state === "unavailable"}
                <span>{manage_history_reasoning_flag()}</span>
              {/if}
            </div>
            {#if carries || showsThought}
              <div class="space-y-1 rounded bg-surface-100-900 p-2">
                {#if carries}
                  {#key bodyKey(turn.turn_id, slot.index, "text")}
                    <StoredText
                      text={view.text}
                      more={view.more}
                      body={bodies[bodyKey(turn.turn_id, slot.index, "text")]}
                      kind="prose"
                      onRead={(offset) =>
                        onReadMessage(
                          turn.turn_id,
                          slot.index,
                          "text",
                          slot.message.role,
                          offset,
                        )}
                      onMore={(offset) =>
                        onMoreMessage(
                          turn.turn_id,
                          slot.index,
                          "text",
                          offset,
                          slot.message.role,
                        )}
                    />
                  {/key}
                {/if}
                {#if showsThought}
                  <div class="opacity-80">
                    {#key bodyKey(turn.turn_id, slot.index, "reasoning")}
                      <StoredText
                        text={thought.text}
                        more={thought.more}
                        body={bodies[
                          bodyKey(turn.turn_id, slot.index, "reasoning")
                        ]}
                        kind="prose"
                        onRead={(offset) =>
                          onReadMessage(
                            turn.turn_id,
                            slot.index,
                            "reasoning",
                            slot.message.role,
                            offset,
                          )}
                        onMore={(offset) =>
                          onMoreMessage(
                            turn.turn_id,
                            slot.index,
                            "reasoning",
                            offset,
                            slot.message.role,
                          )}
                      />
                    {/key}
                  </div>
                {/if}
              </div>
            {:else if thought.state === "unavailable"}
              <p class="text-xs opacity-60">
                {manage_history_reasoning_unavailable()}
              </p>
            {/if}
          </div>
        {/each}
      </div>
    {/if}

    {#if ranTools(turn)}
      <div class="space-y-2">
        <p class="text-xs font-semibold text-surface-800-200">
          {manage_history_calls_label()}
        </p>
        {#each turn.calls as call (call.call_index)}
          <TurnCall
            {call}
            turnId={turn.turn_id}
            {bodies}
            onRead={(part, offset) =>
              onReadBody(turn.turn_id, call.call_index, part, offset)}
            onMore={(part, offset) =>
              onMoreBody(turn.turn_id, call.call_index, part, offset)}
          />
        {/each}
      </div>
    {/if}
  {/if}
</div>
