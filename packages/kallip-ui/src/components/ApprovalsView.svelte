<script lang="ts">
  import type { ApprovalEntry } from "@kallipai/kallip-common";
  import ApprovalRow from "./ApprovalRow.svelte";

  let {
    approvals,
    error,
    loaded,
    onrefresh,
    onapprove,
    ondeny,
  }: {
    approvals: ApprovalEntry[];
    error: string | null;
    loaded: boolean;
    onrefresh: () => Promise<void>;
    onapprove: (id: string) => Promise<void>;
    ondeny: (id: string, reason?: string) => Promise<void>;
  } = $props();

  let refreshing = $state(false);

  // Count of actionable approvals (committed = awaiting human review).
  const committedCount = $derived(
    approvals.filter((a) => a.status === "committed").length,
  );

  async function refresh() {
    refreshing = true;
    try {
      await onrefresh();
    } finally {
      refreshing = false;
    }
  }
</script>

<div class="h-full overflow-y-auto p-6 space-y-3">
  <div class="flex items-center gap-3">
    <button
      class="btn btn-sm preset-tonal-surface"
      onclick={refresh}
      disabled={refreshing}
    >
      {refreshing ? "Refreshing…" : "Refresh"}
    </button>
    {#if committedCount > 0}
      <span class="badge preset-filled-warning-500"
        >{committedCount} awaiting review</span
      >
    {/if}
  </div>

  {#if error}
    <div class="text-sm text-error-500">{error}</div>
  {/if}

  {#if !loaded && !error}
    <p class="opacity-70 text-sm">Loading…</p>
  {:else if loaded && approvals.length === 0}
    <p class="opacity-70 text-sm">No approvals yet.</p>
  {/if}

  {#each approvals as approval (approval.id)}
    <div class="card preset-tonal-surface p-3">
      <ApprovalRow {approval} {onapprove} {ondeny} />
    </div>
  {/each}
</div>
