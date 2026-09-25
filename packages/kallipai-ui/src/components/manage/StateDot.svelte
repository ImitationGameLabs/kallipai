<script lang="ts">
  // State indicator mark for the manage surfaces: the shared icon
  // rendering (shape + motion + color, never color alone). Same table
  // as every other surface, so all faces draw identical marks; 14px
  // keeps retrying's centered mark legible at table scale.

  import AgentStateIcon from "../AgentStateIcon.svelte";
  import type { AgentLifecycleState } from "../../lib/agentState.ts";

  let { state }: { state: AgentLifecycleState } = $props();
</script>

<AgentStateIcon {state} size="size-3.5" />
