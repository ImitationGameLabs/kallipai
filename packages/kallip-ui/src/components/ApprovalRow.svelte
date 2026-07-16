<script lang="ts">
  import type { ApprovalEntry, ApprovalStatus } from "@kallipai/kallip-common";

  let {
    approval,
    onapprove,
    ondeny,
  }: {
    approval: ApprovalEntry;
    onapprove: (id: string) => Promise<void>;
    ondeny: (id: string, reason?: string) => Promise<void>;
  } = $props();

  let denyReason = $state("");
  let submitting = $state(false);
  let rowError = $state<string | null>(null);

  function messageOf(e: unknown): string {
    return e instanceof Error ? e.message : String(e);
  }

  // Badge preset per status: committed is actionable (warning),
  // approved/redeemed are success terminals, denied is an error, and the rest
  // (pending, cancelled) are informational.
  function badgeClass(status: ApprovalStatus): string {
    switch (status) {
      case "committed":
        return "preset-filled-warning-500";
      case "approved":
      case "redeemed":
        return "preset-filled-success-500";
      case "denied":
        return "preset-filled-error-500";
      default:
        return "preset-tonal-surface";
    }
  }

  async function approve() {
    submitting = true;
    rowError = null;
    try {
      await onapprove(approval.id);
    } catch (e) {
      rowError = messageOf(e);
    } finally {
      submitting = false;
    }
  }

  async function deny() {
    submitting = true;
    rowError = null;
    try {
      await ondeny(approval.id, denyReason.trim() || undefined);
    } catch (e) {
      rowError = messageOf(e);
    } finally {
      submitting = false;
    }
  }
</script>

<div class="space-y-2">
  <div class="flex items-center gap-2">
    <span class="font-mono text-sm font-semibold"
      >{approval.content.toolName}</span
    >
    <span class="badge {badgeClass(approval.status)}">{approval.status}</span>
  </div>

  <div class="text-xs opacity-60">
    {approval.requestedBy} · {approval.createdAt}
  </div>

  {#if approval.commitReason}
    <div class="text-sm">{approval.commitReason}</div>
  {/if}

  <details>
    <summary class="text-xs opacity-60 cursor-pointer">arguments</summary>
    <pre class="whitespace-pre-wrap font-mono text-xs">{JSON.stringify(
        approval.content.arguments,
        null,
        2,
      )}</pre>
  </details>

  {#if approval.denyReason}
    <div class="text-sm text-error-500">{approval.denyReason}</div>
  {/if}

  {#if approval.status === "committed"}
    <div class="flex items-center gap-2">
      <button
        class="btn btn-sm preset-filled-primary-500"
        onclick={approve}
        disabled={submitting}
      >
        Approve
      </button>
      <input
        class="input flex-1"
        bind:value={denyReason}
        placeholder="deny reason (optional)"
        disabled={submitting}
      />
      <button
        class="btn btn-sm preset-filled-error-500"
        onclick={deny}
        disabled={submitting}
      >
        Deny
      </button>
    </div>
  {/if}

  {#if rowError}
    <div class="text-xs text-error-500">{rowError}</div>
  {/if}
</div>
