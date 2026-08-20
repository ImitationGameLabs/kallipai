<script lang="ts">
  // State indicator glyph: shape + color carry the state, never color alone
  // (◌ idle / ● busy / ● waiting pulsing / ● retrying / ▲ parked / ✕
  // faulted). Same shared table as the status card rows, so every surface
  // renders identical marks.

  import {
    agentStateGlyph,
    type AgentLifecycleState,
  } from "../../lib/agentState.ts";

  let { state }: { state: AgentLifecycleState } = $props();

  const glyph = $derived(agentStateGlyph(state));
</script>

<span
  class="text-sm leading-none shrink-0 {glyph.className}"
  aria-hidden="true"
>{glyph.char}</span>
