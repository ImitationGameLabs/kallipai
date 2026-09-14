<script lang="ts">
  // The one rendering site for an agent's state mark: the shared table in
  // lib/agentState.ts supplies the Lucide component, its color/motion
  // classes, and an optional centered overlay (retrying's "!"), so every
  // surface -- top-bar pills, detail layer, manage table, agent rows,
  // drawer -- draws identical shapes. `label` renders the accessible
  // name (role="img" + tooltip); decorative callers nested inside an
  // already-labelled control pass false.
  import {
    agentStateIcon,
    agentStateLabel,
    type AgentLifecycleState,
  } from "../lib/agentState.ts";

  let {
    state,
    size = "size-5",
    label = true,
  }: {
    state: AgentLifecycleState;
    /** Tailwind size classes for the mark's box (default: 20px). */
    size?: string;
    label?: boolean;
  } = $props();

  const spec = $derived(agentStateIcon(state));
</script>

<span
  class="relative inline-grid place-items-center leading-none shrink-0 {size}"
  title={label ? agentStateLabel(state) : undefined}
  role={label ? "img" : undefined}
  aria-label={label ? agentStateLabel(state) : undefined}
>
  <spec.comp
    class="size-full {spec.colorClassName} {spec.motionClassName ?? ''}"
    aria-hidden="true"
  />
  {#if spec.center}
    <!-- The overlay takes the tone pair only, never the motion
         classes: the mark holds still while the ring spins, and
         motion-reduce environments read the shape either way. Fixed
         8px keeps the mark legible down to the 14px table scale. -->
    <span
      class="absolute inset-0 grid place-items-center font-bold text-[0.5rem] {spec.colorClassName}"
      aria-hidden="true">{spec.center}</span
    >
  {/if}
</span>
