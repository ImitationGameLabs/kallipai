<script lang="ts" module>
  // Tone -> Skeleton `preset-filled-*` utility. Each bundles the solid tone
  // fill with its paired contrast text, so the banner reads boldly, never lets
  // the content beneath bleed through, and stays in lockstep with Skeleton's
  // own component styling. Add tones here as the design calls for them; `error`
  // is the only one wired today.
  export const bannerTones = {
    error: "preset-filled-error-500",
    warning: "preset-filled-warning-500",
    info: "preset-filled-primary-500",
  } as const satisfies Record<string, string>;

  export type BannerTone = keyof typeof bannerTones;
</script>

<script lang="ts">
  import type { Snippet } from "svelte";

  let {
    title,
    detail = undefined,
    hint = undefined,
    tone = "error",
    icon = undefined,
    floating = false,
    children,
  }: {
    title: string;
    detail?: string;
    hint?: string;
    tone?: BannerTone;
    // Optional leading icon (e.g. an @lucide/svelte component); kept caller-
    // supplied so this package stays free of an icon dependency.
    icon?: Snippet<[{ class: string }]>;
    // Detach from document flow and hover at the top of the viewport instead of
    // sitting inline above page content. Used by the auth pages (rendered
    // outside AppShell) so an agora-unreachable error floats over the centered
    // form with breathing room from the top edge.
    floating?: boolean;
    children?: Snippet;
  } = $props();

  // `tone` is typed as BannerTone but a caller could still pass an unknown
  // string at the boundary; fall back to `error` rather than rendering unstyled.
  const toneClass = $derived(bannerTones[tone] ?? bannerTones.error);

  // Inline: centered, in flow, scaled up and dropped below the chrome so it
  // reads as a prominent notice (the offline tagma-error banner). Floating:
  // detached, fixed well below the viewport top, for surfaces without AppShell
  // chrome (the auth pages' agora-unreachable banner).
  const rootClass = $derived(
    floating
      ? "fixed inset-x-0 top-24 z-50 flex justify-center px-4 text-base md:text-2xl"
      : "flex justify-center px-4 pt-10 text-xl",
  );
  const boxClass = $derived(
    floating
      ? `flex flex-wrap items-center justify-center gap-x-3 gap-y-1.5 max-w-prose text-center rounded-2xl px-5 py-4 md:px-10 md:py-6 shadow-2xl ring-1 ring-black/10 ${toneClass}`
      : `flex flex-wrap items-center justify-center gap-x-2.5 gap-y-1 max-w-prose text-center rounded-xl px-6 py-4 shadow-lg ring-1 ring-black/5 ${toneClass}`,
  );
</script>

<!--
  Centered top banner. Renders as a single condition pill (e.g. "Couldn't reach
  the tagma"); the tint and rounding live on the inner box so the banner reads
  as a discrete centered element, not a full-width bar. Content is capped so
  long messages wrap inside a readable measure. Inline by default; `floating`
  lifts it to a fixed viewport-top position for surfaces without AppShell chrome.
-->
<div role="alert" class={rootClass}>
  <div class={boxClass}>
    {#if icon}{@render icon({ class: "size-4 shrink-0" })}{/if}
    <span class="font-medium">{title}</span>
    {#if detail}<span class="opacity-80">{detail}</span>{/if}
    {#if hint}<span class="opacity-60">{hint}</span>{/if}
    {@render children?.()}
  </div>
</div>
