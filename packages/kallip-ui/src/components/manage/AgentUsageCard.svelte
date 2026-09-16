<script lang="ts">
  // Single-launch token usage for this agent: the prompt-token total,
  // the cache-read counter drawn from it, and the server-computed hit
  // rate. The scope differs from the context card
  // beside it (lifetime cumulative totals), so this is a separate card —
  // sharing the context grid would put two cache-read numbers under one
  // heading. Counters reset when the tagma process restarts.
  import { formatTokenCount } from "../../lib/tagmata.svelte.ts";
  import type { AgentUsageStats } from "@kallipai/kallip-client";
  import {
    manage_agent_usage_heading,
    manage_agent_usage_prompt_tokens,
    manage_agent_usage_cache_read,
    manage_agent_usage_hit_rate,
  } from "../../paraglide/messages.js";

  let { usage }: { usage: AgentUsageStats } = $props();
</script>

<section class="card preset-tonal-surface p-5 space-y-3">
  <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
    {manage_agent_usage_heading()}
  </h2>
  <div class="grid grid-cols-2 gap-3 text-sm">
    <div>
      <span class="opacity-60 text-xs"
        >{manage_agent_usage_prompt_tokens()}</span
      ><span class="font-medium ml-2"
        >{formatTokenCount(usage.prompt_tokens)}</span
      >
    </div>
    <div>
      <span class="opacity-60 text-xs">{manage_agent_usage_cache_read()}</span
      ><span class="font-medium ml-2"
        >{formatTokenCount(usage.cache_read_tokens)}</span
      >
    </div>
    <div>
      <span class="opacity-60 text-xs">{manage_agent_usage_hit_rate()}</span
      ><span class="font-medium ml-2"
        >{(usage.cache_hit_rate * 100).toFixed(1)}%</span
      >
    </div>
  </div>
</section>
