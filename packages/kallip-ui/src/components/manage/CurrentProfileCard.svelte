<script lang="ts">
  import type { ActiveProfile } from "@kallipai/kallip-client";
  import {
    manage_agent_current_profile,
    manage_agent_profile,
    manage_agent_model,
    manage_agent_provider,
    manage_agent_tier,
  } from "../../paraglide/messages.js";

  let { profile }: { profile: ActiveProfile } = $props();
</script>

<section class="card preset-tonal-surface p-5 space-y-3">
  <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
    {manage_agent_current_profile()}
  </h2>
  <div class="grid grid-cols-2 gap-3 text-sm">
    <div>
      <span class="opacity-60 text-xs uppercase tracking-wide block"
        >{manage_agent_profile()}</span
      >
      <span class="font-mono text-xs break-all block">{profile.profile_id}</span
      >
    </div>
    <div>
      <span class="opacity-60 text-xs uppercase tracking-wide block"
        >{manage_agent_model()}</span
      >
      <span class="font-medium truncate block">{profile.model}</span>
    </div>
    <div>
      <span class="opacity-60 text-xs uppercase tracking-wide block"
        >{manage_agent_provider()}</span
      >
      <span class="font-mono text-xs break-all block">{profile.provider}</span>
    </div>
    <div>
      <span class="opacity-60 text-xs uppercase tracking-wide block"
        >{manage_agent_tier()}</span
      >
      <span class="font-mono">#{profile.tier_index}</span>
    </div>
  </div>
</section>
