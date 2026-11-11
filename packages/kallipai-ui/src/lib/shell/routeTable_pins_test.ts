// Wiring pins for the route table, read from the one file that owns the
// wiring. deno test cannot load .svelte components, so these read sources;
// every fragment is one line, so a reformat cannot turn them red.
import { assert, assertEquals } from "@std/assert";
import { ROUTE_PATTERNS } from "./routePatterns.ts";

const TABLE = new URL("./routeTable.ts", import.meta.url);
const VIEW = new URL("./RouteView.svelte", import.meta.url);
const ADMIN_AREA = new URL(
  "../../components/AdminArea.svelte",
  import.meta.url,
);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "a permanent redirect is a 301",
  { permissions: { read: [TABLE] } },
  () => {
    assert(
      source(TABLE).includes("return { status: 301, to };"),
      "permanent redirects must answer 301, not 302",
    );
  },
);

Deno.test(
  "the /admin gate keys off the row, not the path",
  { permissions: { read: [VIEW] } },
  () => {
    const view = source(VIEW);
    assert(view.includes("import AdminArea from"), "the view owns the gate");
    assert(
      view.includes('current.area === "admin"'),
      "the gate reads the row's area",
    );
    assert(view.includes("<AdminArea>"), "area rows render inside AdminArea");
  },
);

Deno.test(
  "each row is one page instance",
  { permissions: { read: [VIEW] } },
  () => {
    // Without the key, two rows sharing a component would reuse one
    // instance and carry its local state across, and every check stays
    // green: this pin is the only thing holding the remount.
    assert(
      source(VIEW).includes("{#key current.pattern}"),
      "a row change must remount the page, a param change must not",
    );
  },
);

Deno.test(
  "AdminArea keeps the panorama card's gateway predicate",
  { permissions: { read: [ADMIN_AREA] } },
  () => {
    const area = source(ADMIN_AREA);
    assert(area.includes('shellMode() === "online"'), "online shells only");
    assert(
      area.includes("archeionSession.user?.local_admin === true"),
      "local admins only",
    );
    assert(
      area.includes("admin_area_forbidden()"),
      "the refused posture carries the 403 note",
    );
  },
);

Deno.test(
  "the login row reads the injected identity, not the build",
  { permissions: { read: [TABLE] } },
  () => {
    assert(
      source(TABLE).includes("offlineLogin: shellIdentity().offlineLogin"),
      "the login form's offline flag comes from the host's initShellIdentity",
    );
  },
);

Deno.test(
  "the two admin gateway rows carry the gate",
  { permissions: { read: [TABLE] } },
  () => {
    const table = source(TABLE);
    // The gate is a data field: losing it renders the admin console for
    // everyone while every other check stays green.
    assertEquals(
      table.match(/area: "admin"/g)?.length ?? 0,
      2,
      "exactly the two admin gateway rows are gated",
    );
    assert(
      table.includes('pattern: "admin/gateway"') &&
        table.includes('"admin/gateway/collections/[name]"'),
      "the gated rows are the admin gateway pages",
    );
  },
);

Deno.test(
  "the table writes one row per URL in the list",
  { permissions: { read: [TABLE] } },
  () => {
    const table = source(TABLE);
    // Two rows with the same pattern collapse into one member of the
    // pattern union, so the second row never renders while every other
    // check stays green. Counting the rows against the URL list turns
    // that, a dropped row, and a row added without a list entry red.
    // Each row states its pattern as one literal, except the six manage
    // sections, which a map expands from their own list.
    const literals = table.match(/pattern: "/g)?.length ?? 0;
    const sectionList = table.match(/const TagmaDetailsSections[^;]*?\];/);
    assert(
      sectionList !== null,
      "the manage section list is where the table reads it",
    );
    const body = sectionList[0].replace(/[/][/].*/g, "");
    const sectionRows = body.match(/"[^"]+"/g)?.length ?? 0;
    assertEquals(
      literals + sectionRows,
      ROUTE_PATTERNS.length,
      "each row states one pattern, and the list holds one URL per row",
    );
  },
);
