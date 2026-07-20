<script lang="ts">
  // The kallipai wordmark. The brand is "kallip" + "ai"; the `AI` is accented in
  // the theme primary (a cool blue under rocket) to surface that composition
  // and stay consistent with buttons, links, and the focus ring. Single source
  // of truth -- the AppShell sidebar default, the login page, and the register
  // page all render this so the trademark reads consistently across the app.
  let { size = "md" }: { size?: "md" | "lg" | "xl" } = $props();
  const cls = $derived(
    size === "xl" ? "text-3xl" : size === "lg" ? "text-2xl" : "text-xl",
  );
</script>

<span class="{cls} font-bold tracking-tight">
  Kallip<span class="text-primary-500">AI</span>
</span>
