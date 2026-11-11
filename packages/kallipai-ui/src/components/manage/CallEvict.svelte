<script lang="ts">
  // context_evict's body: what it condensed, what survived the keep window,
  // and the two configurations that explain an outcome that looks wrong. The
  // numbers are the record's own; the reason a token slice kept nothing is
  // quoted as the record states it.
  import type {
    CallViewProps,
    EvictResult,
  } from "../../lib/manage/toolViews.ts";
  import { evictFacts, evictTone } from "../../lib/manage/toolViews.ts";
  import { formatTokens } from "../../lib/compute.ts";
  import { getLocale } from "../../paraglide/runtime.js";
  import {
    manage_history_evict_cap_zero,
    manage_history_evict_degraded,
    manage_history_evict_kept,
    manage_history_evict_line,
  } from "../../paraglide/messages.js";
  import CallNarration from "./CallNarration.svelte";

  let { payload }: CallViewProps<EvictResult> = $props();

  const facts = $derived(evictFacts(payload));

  /** A call that condensed nothing is not an outcome to celebrate, so its dot
   * stays neutral; one that did condense turns reports a success. */
  const tone = $derived(evictTone(facts));

  const line = $derived(
    manage_history_evict_line({
      evicted: formatTokens(facts.evicted, getLocale()),
      total: formatTokens(facts.total, getLocale()),
      freed: formatTokens(facts.freed, getLocale()),
    }),
  );

  /** The keep window, as far as the record states it. A cap of 0 is explained
   * by its own line, so the window line stays out of that case rather than
   * saying it kept 0 turns under a cap of 0. */
  const keptLine = $derived.by(() => {
    const kept = facts.kept;
    if (kept === null || facts.capZero) return null;
    const caps = {
      keptTurns: formatTokens(kept.keptTurns, getLocale()),
      maxTurns: formatTokens(kept.maxTurns, getLocale()),
      maxTokens: formatTokens(kept.maxTokens, getLocale()),
    };
    return manage_history_evict_kept(caps);
  });
</script>

<div class="space-y-1">
  <CallNarration {tone} text={line} />
  {#if keptLine !== null}
    <p class="pl-3.5 text-xs text-surface-800-200">{keptLine}</p>
  {/if}
  {#if facts.capZero}
    <p class="pl-3.5 text-xs text-surface-800-200">
      {manage_history_evict_cap_zero()}
    </p>
  {/if}
  {#if facts.degraded !== null}
    <p class="pl-3.5 text-xs text-surface-800-200">
      {manage_history_evict_degraded({ reason: facts.degraded })}
    </p>
  {/if}
</div>
