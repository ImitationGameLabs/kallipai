<script lang="ts">
  // Colored state indicator dot.
  // green: idle, blue: busy, red: faulted.

  let { state }: { state: "idle" | "busy" | "faulted" } = $props();

  const dotClass = $derived(
    state === "idle"
      ? "bg-success-500"
      : state === "busy"
        ? "bg-info-500"
        : "bg-error-500",
  );
</script>

<span
  class="size-2 rounded-full shrink-0 {dotClass}"
  aria-hidden="true"
></span>
