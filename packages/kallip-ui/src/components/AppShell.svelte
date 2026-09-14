<script lang="ts">
  import type { Snippet } from "svelte";
  import type { NavSection } from "../lib/shell/links.ts";
  import { desktopQuery } from "../lib/shell/breakpoint.ts";
  import type { ErrorView } from "../lib/errors.ts";

  let {
    links,
    isActive,
    brand,
    status,
    back = null,
    topRow = undefined,
    title = undefined,
    pathname,
    error = null,
    topPanel = undefined,
    children,
  }: {
    links: NavSection[];
    /** The current pathname: the trail table (lib/shell/breadcrumbs.ts)
     * is keyed on it, so the bar needs no per-page wiring. */
    pathname: string;
    // Consumer-supplied route matcher ("/" exact, others by prefix). Kept out of
    // the shell so deep links such as `/chat/:id` still resolve.
    isActive: (href: string) => boolean;
    // Optional chrome snippets: `brand` and `status` dress the desktop
    // sidebar (header and footer); `topRow`/`topPanel`/`title` dress the
    // mobile top rows. A non-null `back` swaps the mobile bar for the
    // back row (see MobileShell).
    brand?: Snippet;
    status?: Snippet;
    back?: { href: string; label: string } | null;
    topRow?: Snippet;
    title?: string;
    error?: ErrorView | null;
    topPanel?: Snippet;
    children: Snippet;
  } = $props();

  // The single structural fork: one matchMedia at the Tailwind md
  // breakpoint (48rem) mounts exactly one shell. The shells carry no
  // media queries -- being mounted IS the viewport verdict -- so the
  // old pair of always-alive Navigation instances toggled by
  // hidden/md:grid classes is gone, and with it the duplicate nav tree
  // in the DOM below md. Crossing the breakpoint swaps the mounted
  // shell; shell-local transients (the More sheet and its open state)
  // die with the unmount, which is the active-zeroing the old md
  // listener used to enforce. The dynamic imports keep each shell in
  // its own chunk, so a session on one form does not pay for the
  // other's markup.
  const mdQuery = matchMedia(desktopQuery);
  let desktop = $state(mdQuery.matches);
  $effect(() => {
    const onChange = (event: MediaQueryListEvent) => {
      desktop = event.matches;
    };
    mdQuery.addEventListener("change", onChange);
    return () => mdQuery.removeEventListener("change", onChange);
  });

  // One prop bundle for both shells: they partition the surface (the
  // desktop half ignores the top-row snippets, the mobile half ignores
  // brand/status), and the extra keys are simply never read.
  const shellProps = $derived({
    links,
    isActive,
    brand,
    status,
    back,
    topRow,
    title,
    pathname,
    error,
    topPanel,
    children,
  });
</script>

{#snippet chunkFallback()}
  <!-- A failed lazy import is a stale-deploy edge: the other shell's
       hashed chunk is gone for this session. One manual reload
       re-bootstraps from the fresh index; no auto-reload, so a
       persistent failure cannot loop. Plain text like the Loading…
       chrome exception (no new i18n keys). -->
  <div class="p-4">
    <p class="opacity-60">
      The interface failed to load.
      <button type="button" class="underline" onclick={() => location.reload()}>
        Reload
      </button>
    </p>
  </div>
{/snippet}
{#if desktop}
  {#await import("../lib/shell/DesktopShell.svelte") then { default: DesktopShell }}
    <DesktopShell {...shellProps} />
  {:catch}
    {@render chunkFallback()}
  {/await}
{:else}
  {#await import("../lib/shell/MobileShell.svelte") then { default: MobileShell }}
    <MobileShell {...shellProps} />
  {:catch}
    {@render chunkFallback()}
  {/await}
{/if}
