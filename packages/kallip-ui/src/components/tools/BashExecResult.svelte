<script lang="ts">
  import MonoBlock from "./MonoBlock.svelte";

  // BashExecOutput (crates/kallip-shell/src/tools/bash_exec.rs:36-64). All stream
  // fields optional; exit_code null on signal death, 124 on timeout. A present
  // task_id means a background spawn (always a success regardless of exit code).
  interface BashExecOutput {
    output?: string;
    stdout?: string;
    stderr?: string;
    exit_code?: number | null;
    timed_out?: boolean;
    truncated?: boolean;
    cwd?: string;
    task_id?: string;
  }

  let { result }: { result: unknown } = $props();

  const out = $derived((result ?? {}) as BashExecOutput);
  // Capture mode decides which stream field holds the text.
  const stream = $derived(out.output ?? out.stdout ?? out.stderr ?? "");
  const isBackground = $derived(typeof out.task_id === "string");
  // Foreground failure rule (executor.rs:274-284): non-zero OR null exit.
  const failed = $derived(!isBackground && out.exit_code !== 0);
  const exitBadge = $derived(
    out.exit_code === null || out.exit_code === undefined
      ? "no exit"
      : `exit ${out.exit_code}`,
  );
</script>

<div class="space-y-1.5">
  <div class="flex flex-wrap items-center gap-1.5 text-xs">
    <span
      class="badge {failed
        ? 'preset-filled-error-500'
        : 'preset-filled-success-500'}"
    >
      {exitBadge}
    </span>
    {#if isBackground}<span class="badge preset-tonal-surface">background</span
      >{/if}
    {#if out.timed_out}<span class="badge preset-filled-warning-500"
        >timed out</span
      >{/if}
    {#if out.truncated}<span class="badge preset-filled-warning-500"
        >truncated</span
      >{/if}
    {#if out.task_id}<span class="font-mono opacity-60">{out.task_id}</span
      >{/if}
  </div>
  {#if stream}
    <MonoBlock text={stream} />
  {/if}
  {#if out.cwd}<div class="text-xs opacity-50 font-mono break-all">
      {out.cwd}
    </div>{/if}
</div>
