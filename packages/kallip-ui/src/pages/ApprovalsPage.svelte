<script lang="ts">
  import type { Session } from "@kallipai/kallip-common";
  import { sessionStore } from "../lib/session/session.svelte";
  import ApprovalsView from "../components/ApprovalsView.svelte";

  // Seed approvals exactly once per session. Uses the previous-value pattern so
  // the effect never writes state it reads: the only state it writes is local
  // `seeded`, and the write always changes the value, so it converges. Also
  // re-seeds on reconnect, since attach()'s finally nulls the session.
  let seeded: Session | null = $state(null);
  $effect(() => {
    const s = sessionStore.session;
    if (s && s !== seeded) {
      seeded = s;
      void sessionStore.refreshApprovals();
    }
  });
</script>

<svelte:head><title>KallipAI · approvals</title></svelte:head>

{#if sessionStore.connected}
  <div class="flex flex-col h-full">
    <div class="flex-1 min-h-0">
      <ApprovalsView
        approvals={sessionStore.approvals}
        error={sessionStore.approvalsError}
        loaded={sessionStore.approvalsLoaded}
        onrefresh={() => sessionStore.refreshApprovals()}
        onapprove={(id) => sessionStore.approve(id)}
        ondeny={(id, reason) => sessionStore.deny(id, reason)}
      />
    </div>
  </div>
{/if}
