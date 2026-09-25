import { assert } from "@std/assert";

// Source-read pins for the shared ManageGrid mount.
// The hub pages have no runtime tests, and neither svelte-check nor lint
// flags a component that is imported but never rendered -- the gap let an
// empty <nav></nav> land on /local/manage with the whole suite green.
// These pins guard the mount itself: the template (past </script>) must
// actually carry the component, and the grid must carry the six hrefs.

const MANAGE_HUB = new URL("./manage/ManageHubPage.svelte", import.meta.url);
const LOCAL_HOME = new URL("./LocalHomePage.svelte", import.meta.url);
const MANAGE_GRID = new URL("../components/ManageGrid.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

const HREFS = [
  "/local/manage/overview",
  "/local/manage/budget",
  "/local/manage/agents",
  "/local/manage/profiles",
  "/local/manage/schedules",
  "/local/manage/tasks",
];

Deno.test(
  "ManageHubPage mounts the shared ManageGrid in its template",
  { permissions: { read: [MANAGE_HUB] } },
  () => {
    const src = source(MANAGE_HUB);
    const mount = src.indexOf("<ManageGrid", src.indexOf("</script>"));
    assert(mount !== -1, "the nav must render ManageGrid, not import it only");
  },
);

Deno.test(
  "LocalHomePage mounts the shared ManageGrid in its template",
  { permissions: { read: [LOCAL_HOME] } },
  () => {
    const src = source(LOCAL_HOME);
    const mount = src.indexOf("<ManageGrid", src.indexOf("</script>"));
    assert(mount !== -1, "the manage section must render ManageGrid");
  },
);

Deno.test(
  "ManageGrid carries the six manage hrefs",
  { permissions: { read: [MANAGE_GRID] } },
  () => {
    const src = source(MANAGE_GRID);
    for (const href of HREFS) {
      assert(src.includes(href), `the grid must list ${href}`);
    }
  },
);
