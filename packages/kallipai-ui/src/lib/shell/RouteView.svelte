<script lang="ts">
  // Renders the row the route table picked for this pathname. Both host apps
  // keep one rest route (`[...path]`) and pass it the current pathname and
  // search; redirects and 404s already happened in that route's load, so this
  // view only picks the component and its props, and applies the /admin gate.
  //
  // A row with a component renders even while its redirect condition would
  // now hold: crossing the breakpoint after mount belongs to the page (the
  // panorama moves itself), and rendering nothing until the next navigation
  // would leave a blank screen that nothing fills. A redirect-only row does
  // render nothing, because that route's load has already sent the browser
  // elsewhere; reaching here means the navigation was superseded.
  //
  // One row is one page instance: the key is the row's pattern, so a move
  // between rows remounts the component and a param change inside one row
  // reuses it, as kit's own routing does.
  //
  // The layout is the host's: this view renders the page body only. The
  // unsaved-changes guard lives in the manage overlay, which the base layer
  // must not import, so the host mounts it next to this view.
  import AdminArea from "../../components/AdminArea.svelte";
  import { findRoute } from "./routeTable.ts";

  let {
    pathname,
    search,
  }: {
    pathname: string;
    search: string;
  } = $props();

  const found = $derived(findRoute(pathname));
  const current = $derived(
    found !== null && "component" in found.row ? found.row : null,
  );
  const values = $derived(
    current === null
      ? {}
      : (current.props?.({
          params: found?.params ?? {},
          search: new URLSearchParams(search),
        }) ?? {}),
  );
</script>

{#if current}
  {@const Page = current.component}
  {#key current.pattern}
    {#if current.area === "admin"}
      <AdminArea>
        <Page {...values} />
      </AdminArea>
    {:else}
      <Page {...values} />
    {/if}
  {/key}
{/if}
