<script lang="ts">
  import "../app.css";
  import { page } from "$app/state";
  import SiteFooter from "$lib/components/SiteFooter.svelte";
  import SiteHeader from "$lib/components/SiteHeader.svelte";
  import { ogImagePath, siteUrl } from "$lib/site";

  let { children } = $props();

  // $derived: the root layout survives client-side navigation, so the
  // canonical must track the pathname instead of freezing at first render.
  const canonical = $derived(
    siteUrl + (page.url.pathname === "/" ? "/en/" : page.url.pathname),
  );
</script>

<svelte:head>
  <!-- Card defaults for every page; a page's own head block overrides
      title and description, and a load returning seoTitle /
      seoDescription gets those promoted to og:title / og:description
      here (the docs pages do this). -->
  <link rel="canonical" href={canonical} />
  <meta property="og:url" content={canonical} />
  <meta property="og:site_name" content="KallipAI" />
  <meta property="og:type" content="website" />
  <!-- og.png is a placeholder path: the card image is not designed yet. -->
  <meta property="og:image" content={siteUrl + ogImagePath} />
  <meta name="twitter:card" content="summary" />
  {#if page.data.seoTitle !== undefined}
    <meta property="og:title" content={page.data.seoTitle} />
  {/if}
  {#if page.data.seoDescription !== undefined}
    <meta property="og:description" content={page.data.seoDescription} />
  {/if}
</svelte:head>

<div class="flex min-h-screen flex-col">
  <SiteHeader />
  <main class="flex-1">
    {@render children()}
  </main>
  <SiteFooter />
</div>
