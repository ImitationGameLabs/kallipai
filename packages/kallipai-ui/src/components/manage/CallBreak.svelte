<script lang="ts">
  // break's body: one line of narration saying how the parked turn will wake.
  // The record says which rule it parked under; one that never carried a rule
  // says so, rather than showing a timer this build guessed at.
  import type {
    CallViewProps,
    BreakResult,
  } from "../../lib/manage/toolViews.ts";
  import { breakFacts } from "../../lib/manage/toolViews.ts";
  import {
    manage_turns_break_message,
    manage_turns_break_not_paused,
    manage_turns_break_timer,
    manage_turns_break_timer_seconds,
    manage_turns_break_unrecorded,
    manage_turns_break_wait,
  } from "../../paraglide/messages.js";
  import CallNarration from "./CallNarration.svelte";

  let { payload }: CallViewProps<BreakResult> = $props();

  const facts = $derived(breakFacts(payload));
  const tone = $derived(facts.state === "not-paused" ? "muted" : "info");

  const line = $derived.by(() => {
    switch (facts.state) {
      case "not-paused":
        return manage_turns_break_not_paused();
      case "paused-message":
        return manage_turns_break_message();
      case "paused-unrecorded":
        return manage_turns_break_unrecorded();
      case "paused-timer": {
        const timer = facts.timer;
        if (timer === null) return manage_turns_break_wait();
        return timer.unit === "seconds"
          ? manage_turns_break_timer_seconds({ seconds: `${timer.value}` })
          : manage_turns_break_timer({ minutes: `${timer.value}` });
      }
    }
  });
</script>

<CallNarration {tone} text={line} />
