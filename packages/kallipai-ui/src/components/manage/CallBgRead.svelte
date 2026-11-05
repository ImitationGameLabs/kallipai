<script lang="ts">
  // bash_background_read's body: how the task stands, and the lines this read
  // returned. The state word is the record's, and a word this build does not
  // know is shown as written rather than guessed at. The tool answers with the
  // tail of what the task wrote, so a folded block says so: the start of that
  // output is not in this read.
  import type {
    BgReadResult,
    CallViewProps,
  } from "../../lib/manage/toolViews.ts";
  import {
    bgReadFacts,
    bgReadTone,
    formatBytes,
  } from "../../lib/manage/toolViews.ts";
  import {
    manage_history_bg_exited,
    manage_history_bg_killed,
    manage_history_bg_running,
    manage_history_bg_stalled,
    manage_history_bg_state_unknown,
    manage_history_bg_tail,
  } from "../../paraglide/messages.js";
  import BlockText from "./BlockText.svelte";
  import CallNarration from "./CallNarration.svelte";

  let { payload }: CallViewProps<BgReadResult> = $props();

  const facts = $derived(bgReadFacts(payload));

  /** The dot reports the state the sentence names; the alarm for a stalled
   * task lives in the line under it. */
  const tone = $derived(bgReadTone(facts));

  const line = $derived.by(() => {
    switch (facts.state) {
      case "running":
        return manage_history_bg_running({ bytes: formatBytes(facts.bytes) });
      case "exited":
        return manage_history_bg_exited({
          code: `${facts.exit}`,
          bytes: formatBytes(facts.bytes),
        });
      case "killed":
        return manage_history_bg_killed({ bytes: formatBytes(facts.bytes) });
      case "other":
        return manage_history_bg_state_unknown({
          state: facts.stateWord,
          bytes: formatBytes(facts.bytes),
        });
    }
  });

  /** A failed command's output is still what the command said; only the box
   * takes the edge, the same way the shared stream block does it. */
  const failed = $derived(facts.exit !== null && facts.exit !== 0);
</script>

<div class="space-y-1">
  <CallNarration {tone} text={line} />
  {#if facts.stalled}
    <p class="pl-3.5 text-xs text-error-900-100">
      {manage_history_bg_stalled()}
    </p>
  {/if}
  {#if facts.folded}
    <p class="pl-3.5 text-xs text-surface-800-200">
      {manage_history_bg_tail()}
    </p>
  {/if}
  {#if facts.tail !== ""}
    <BlockText text={facts.tail} {failed} />
  {/if}
</div>
