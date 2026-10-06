import { assert } from "@std/assert";

// Source pins for the gateway console: the page rides the
// GatewayAdminStore over the shared admin client, renders only inside
// the /admin layout's AdminArea gate (the shell owns the sign-in and
// local-admin postures), keeps the composite edits dialog-scoped,
// carries all five admin domains in-page, and touches no storage.
const PAGE = new URL("./GatewayCatalogPage.svelte", import.meta.url);
const STORE = new URL(
  "../../../lib/manage/gateway/store.svelte.ts",
  import.meta.url,
);
const CLIENT = new URL(
  "../../../lib/manage/gateway/client.ts",
  import.meta.url,
);
const WEB_ROUTE = new URL(
  "../../../../../kallipai-web/src/routes/admin/gateway/+page.svelte",
  import.meta.url,
);
const APP_ROUTE = new URL(
  "../../../../../kallipai-app/src/routes/admin/gateway/+page.svelte",
  import.meta.url,
);
const WEB_COLLECTION_ROUTE = new URL(
  "../../../../../kallipai-web/src/routes/admin/gateway/collections/[name]/+page.svelte",
  import.meta.url,
);
const APP_COLLECTION_ROUTE = new URL(
  "../../../../../kallipai-app/src/routes/admin/gateway/collections/[name]/+page.svelte",
  import.meta.url,
);
const ADMIN_AREA = new URL(
  "../../../components/AdminArea.svelte",
  import.meta.url,
);
const WEB_ADMIN_LAYOUT = new URL(
  "../../../../../kallipai-web/src/routes/admin/+layout.svelte",
  import.meta.url,
);
const APP_ADMIN_LAYOUT = new URL(
  "../../../../../kallipai-app/src/routes/admin/+layout.svelte",
  import.meta.url,
);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "GatewayCatalogPage leaves the session postures to the shell",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    // The route gate owns sign-in and whoami; the /admin layout's
    // AdminArea owns the local-admin posture. The page carries none
    // of them.
    assert(
      !src.includes("archeionSession"),
      "the page no longer reads the session (AdminArea owns it)",
    );
    assert(
      !src.includes("shellMode"),
      "the page no longer branches on the shell mode",
    );
    assert(
      !src.includes("user === undefined") && !src.includes("user === null"),
      "no dead sign-in/loading branch remains (the gate redirects both)",
    );
    assert(!src.includes("/login?next="), "no in-page sign-in pointer remains");
    assert(
      !src.includes("manage_gateway_admin_only") &&
        !src.includes("manage_gateway_sign_in"),
      "the dead defense-layer keys are gone",
    );
    assert(
      !src.includes("manage_gateway_collection_detail_back"),
      "the self-made back link is gone (the trail owns the drill)",
    );
    assert(
      !src.includes("localStorage") && !src.includes("sessionStorage"),
      "nothing persists beyond the session",
    );
  },
);

Deno.test(
  "the /admin layouts gate their subtree on AdminArea",
  { permissions: { read: [ADMIN_AREA, WEB_ADMIN_LAYOUT, APP_ADMIN_LAYOUT] } },
  () => {
    const area = source(ADMIN_AREA);
    assert(
      area.includes('shellMode() === "online"') &&
        area.includes("user?.local_admin === true"),
      "AdminArea keeps the panorama card's gatewayVisible predicate",
    );
    assert(
      area.includes("admin_area_forbidden()"),
      "the refused posture carries the 403 note",
    );
    for (const layout of [WEB_ADMIN_LAYOUT, APP_ADMIN_LAYOUT]) {
      const src = source(layout);
      assert(
        src.includes("<AdminArea") && src.includes("{@render children()}"),
        "the /admin layout wraps its subtree in AdminArea",
      );
    }
  },
);

Deno.test(
  "GatewayCatalogPage edits the catalog through the store, one request per save",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    assert(
      src.includes("gatewayStore.refreshAll()"),
      "the read side loads through the store",
    );
    assert(
      src.includes("gatewayStore.saveSet(") && !src.includes("updateSet("),
      "member and description edits land as the store's single PATCH",
    );
    assert(
      src.includes("gatewayStore.removeSet("),
      "the remove flow routes through the store",
    );
    assert(
      src.includes("<ConfirmDialog"),
      "the destructive remove confirms before it fires",
    );
    assert(
      src.includes("<SetDialog") && src.includes("<SetsSection"),
      "the sets grid and its dialog are the section components",
    );
  },
);

Deno.test(
  "GatewayCatalogPage carries the parking and credential panels",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    // The panels are section components.
    assert(
      src.includes("<ParkingSection"),
      "the parking panel renders as a section component",
    );
    assert(!src.includes("_pending()"), "no pending placeholder rows render");
    // Parking rides the store's filter; a drag into a set rides the
    // same member write as every other placement.
    assert(
      src.includes("gatewayStore.addParked(") &&
        src.includes("gatewayStore.addParkedToSet("),
      "parking actions ride the store's parking wraps",
    );
    // Credentials: the plaintext crosses inbound only (dialog PUT plus
    // a confirmed delete), through the store wraps.
    assert(
      src.includes("gatewayStore.saveCredential(") &&
        src.includes("gatewayStore.removeCredential("),
      "credential writes ride the store's credential wraps",
    );
    assert(
      src.includes("<ProviderCredentialDialog"),
      "the credential form is the dialog, not an inline draft",
    );
    // The admin URL is deployment-baked: the page never edits it
    // and gates the console on its presence.
    assert(
      !src.includes("gatewayStore.adminUrl") && src.includes("isConnected()"),
      "the admin URL is baked config, never a page-editable field",
    );
    // Set creation is the dialog, landed through the store's add wrap.
    assert(
      src.includes("gatewayStore.addSet("),
      "the create-set dialog rides the store's add wrap",
    );
    assert(
      src.includes("<SetCreateDialog") &&
        src.includes("names={(gatewayStore.sets ?? []).map((s) => s.name)}"),
      "the create-set dialog is wired with the catalog's names",
    );
  },
);

Deno.test(
  "GatewayAdminStore wraps the shared client and stores nothing",
  { permissions: { read: [STORE] } },
  () => {
    const src = source(STORE);
    assert(
      src.includes('from "./client.ts"'),
      "the store rides the shared gateway admin client",
    );
    assert(
      !src.includes("adminUrl"),
      "the store carries no URL; the client's baked URL is the only base",
    );
    assert(
      src.includes("export const gatewayStore = new GatewayAdminStore()"),
      "the store is a class singleton (the manage family shape)",
    );
    assert(
      src.includes("await this.run(() => listParking())") &&
        !src.includes("parkingView"),
      "the parking read is the single catalog-space list",
    );
    assert(
      src.includes("parkingBody") && src.includes("listCollectionSets"),
      "parking and the nested sets read are store slices, not page-local state",
    );
    assert(
      !src.includes("localStorage") && !src.includes("sessionStorage"),
      "nothing persists beyond the session",
    );
  },
);

Deno.test(
  "the admin client stores nothing and speaks credentialed HTTP",
  { permissions: { read: [CLIENT] } },
  () => {
    const src = source(CLIENT);
    assert(
      !src.includes("sessionStorage") && !src.includes("localStorage"),
      "no credential is stored client-side anymore",
    );
    assert(
      !src.includes("Bearer"),
      "the bearer header is gone: the session cookie is the credential",
    );
    assert(
      src.includes('credentials: "include"'),
      "every request rides the archeion session cookie",
    );
    assert(
      src.includes("X-Requested-With"),
      "mutating requests carry the CSRF marker",
    );
    assert(
      src.includes("gatewayAdminUrl"),
      "the base URL prefills from the baked runtime config",
    );
  },
);

Deno.test(
  "both shells serve the console at /admin/gateway",
  { permissions: { read: [WEB_ROUTE, APP_ROUTE] } },
  () => {
    for (const route of [WEB_ROUTE, APP_ROUTE] as const) {
      assert(
        source(route).includes("GatewayCatalogPage"),
        "the /admin/gateway route renders the console",
      );
    }
  },
);
Deno.test(
  "the console serves the collection detail as a deep-linked subface",
  {
    permissions: {
      read: [
        PAGE,
        WEB_ROUTE,
        APP_ROUTE,
        WEB_COLLECTION_ROUTE,
        APP_COLLECTION_ROUTE,
      ],
    },
  },
  () => {
    const src = source(PAGE);
    // The page takes the route param as its detail selector.
    assert(
      src.includes("collection = null") && src.includes("detailRow"),
      "the console branches on the collection prop for the detail",
    );
    assert(
      src.includes("detailMemberSets") &&
        src.includes("defaultSet={detailRow.default_set}"),
      "the detail renders its member sets with the default-set anchor",
    );
    assert(
      src.includes(
        "gatewayStore.setCollectionDefault(detailRow.name, set.name)",
      ),
      "the set-as-default action rides the store's transfer wrap",
    );
    assert(
      src.includes("onDropAnywhere") && src.includes("<ParkedPlacementDialog"),
      "the list view's drop opens the placement fallback dialog",
    );
    assert(
      src.includes("navigate(adminGatewayPath())"),
      "a removed collection's detail returns to the list",
    );
    // Both shells bind the param to the same console component.
    for (const route of [WEB_COLLECTION_ROUTE, APP_COLLECTION_ROUTE] as const) {
      const rs = source(route);
      assert(
        rs.includes("GatewayCatalogPage") && rs.includes("page.params.name"),
        "the collections route passes the param to the console",
      );
    }
    // The list entry path: the nav cards anchor into the detail.
    assert(
      src.includes("<CollectionsSection") &&
        src.includes("collections={gatewayStore.collections ?? []}"),
      "the list view renders the collections nav cards",
    );
  },
);
