<script lang="ts">
  // bash_exec's body: one line of narration, the command that ran, and its
  // output. The command comes from the one place that knows where it lives, so
  // no field name is probed here; the output is drawn by the one terminal
  // block, so a stream is read the same way on both faces.
  import type {
    CallViewProps,
    BashExecResult,
  } from "../../lib/manage/toolViews.ts";
  import {
    bashExecFacts,
    shortId,
    shortPath,
  } from "../../lib/manage/toolViews.ts";
  import {
    terminalCommand,
    terminalOutput,
  } from "../../lib/manage/jsonView.ts";
  import {
    manage_history_call_cut,
    manage_history_call_timed_out,
    manage_history_terminal_ran,
    manage_history_terminal_no_exit,
    manage_history_terminal_running,
    manage_history_terminal_running_wait,
  } from "../../paraglide/messages.js";
  import CallNarration from "./CallNarration.svelte";
  import OutputStream from "./OutputStream.svelte";

  let { args, node, payload }: CallViewProps<BashExecResult> = $props();

  const command = $derived(terminalCommand(args));
  const facts = $derived(bashExecFacts(payload));
  const output = $derived(terminalOutput(node));

  const tone = $derived.by(() => {
    switch (facts.state) {
      case "succeeded":
        return "ok" as const;
      case "failed":
        return "error" as const;
      case "running":
        return "info" as const;
      case "no-code":
        return "error" as const;
    }
  });

  /** Where the command ran. How it ended and its exit code are already on
   * the call's own line above, so this face never says them twice. */
  const line = $derived.by(() => {
    switch (facts.state) {
      case "succeeded":
      case "failed":
        return manage_history_terminal_ran({ cwd: shortPath(facts.cwd) });
      case "running": {
        // The `running` state exists only beside a task id, so the fallback
        // below serves the type, not the record.
        const task = shortId(facts.taskId ?? "");
        // A run that is still going has not timed out: say why we stopped
        // waiting instead. The timeout note belongs to calls that ended.
        return facts.timedOut
          ? manage_history_terminal_running_wait({ task })
          : manage_history_terminal_running({ task });
      }
      case "no-code":
        return manage_history_terminal_no_exit();
    }
  });

  /** What the record adds to the outcome, for calls that really ended. */
  const notes = $derived.by(() => {
    if (facts.state === "running") return [];
    const found: string[] = [];
    if (facts.timedOut) found.push(manage_history_call_timed_out());
    if (facts.truncated) found.push(manage_history_call_cut());
    return found;
  });
</script>

<div class="space-y-1">
  <CallNarration
    {tone}
    text={notes.length === 0 ? line : `${line} · ${notes.join(" · ")}`}
  />
  {#if command !== null}
    <p class="break-words font-mono text-xs">
      <span class="opacity-60">$</span>
      {command}
    </p>
  {/if}
  <OutputStream {output} />
</div>
