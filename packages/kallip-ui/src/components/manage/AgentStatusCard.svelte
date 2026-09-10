<script lang="ts">
  // Context-usage card for the agent detail page: the budget bar against
  // the profile's window (hidden while the denominator is unknown -- a bar
  // against a wrong budget lies) plus the six context counters. Receives
  // the already-derived windowTokens/contextWindow pair so the derivation
  // (and its profileConfig dependency) stays in the page.
  // The card accepts `unlimited` (default false) and forwards it to
  // BudgetBar so both callers share the bar's unlimited form (badge
  // overlay, no fill). The context window itself is always finite —
  // a caller pins this to the tagma budget's unlimited flag only if a
  // product decision wants that badge on the context card.

  import BudgetBar from "./BudgetBar.svelte";
  import { formatTokenCount } from "../../lib/tagmata.svelte.ts";
  import type { AgentStatusResponse } from "@kallipai/kallip-client";
  import {
    manage_agent_context_label,
    manage_agent_context_tokens,
    manage_agent_context_usage,
    manage_agent_cumulative_in,
    manage_agent_cumulative_out,
    manage_agent_last_prompt,
    manage_agent_pinned_items,
    manage_agent_turn_tokens,
    manage_agent_turns,
  } from "../../paraglide/messages.js";

  let {
    status,
    windowTokens,
    contextWindow,
    unlimited = false,
  }: {
    status: AgentStatusResponse;
    windowTokens: number;
    contextWindow: number | null;
    unlimited?: boolean;
  } = $props();
</script>

<section class="card preset-tonal-surface p-5 space-y-3">
  <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
    {manage_agent_context_usage()}
  </h2>
  {#if contextWindow}
    <BudgetBar
      consumed={windowTokens}
      budget={contextWindow}
      label={manage_agent_context_label()}
      {unlimited}
    />
    <p class="text-xs opacity-70">
      {manage_agent_context_tokens({
        consumed: formatTokenCount(windowTokens),
        budget: formatTokenCount(contextWindow),
      })}
    </p>
  {:else}
    <!-- window size unknown (profile gone or config not loaded):
         a bar against a wrong denominator lies, so hide the pair -->
  {/if}
  <div class="grid grid-cols-2 gap-3 text-sm">
    <div>
      <span class="opacity-60 text-xs">{manage_agent_turns()}</span><span
        class="font-medium ml-2">{status.context.turn_count}</span
      >
    </div>
    <div>
      <span class="opacity-60 text-xs">{manage_agent_pinned_items()}</span><span
        class="font-medium ml-2">{status.context.pinned_items.length}</span
      >
    </div>
    <div>
      <span class="opacity-60 text-xs">{manage_agent_turn_tokens()}</span><span
        class="font-medium ml-2"
        >{formatTokenCount(status.context.turn_tokens)}</span
      >
    </div>
    <div>
      <span class="opacity-60 text-xs">{manage_agent_last_prompt()}</span><span
        class="font-medium ml-2"
        >{status.context.last_prompt_tokens
          ? formatTokenCount(status.context.last_prompt_tokens)
          : "—"}</span
      >
    </div>
    <div>
      <span class="opacity-60 text-xs">{manage_agent_cumulative_in()}</span
      ><span class="font-medium ml-2"
        >{formatTokenCount(status.context.cumulative_usage.prompt_tokens)}</span
      >
    </div>
    <div>
      <span class="opacity-60 text-xs">{manage_agent_cumulative_out()}</span
      ><span class="font-medium ml-2"
        >{formatTokenCount(
          status.context.cumulative_usage.completion_tokens,
        )}</span
      >
    </div>
  </div>
</section>
