import { assert } from "@std/assert";
import { manage_gateway_remove_collection_body } from "../../../paraglide/messages.js";

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
const UNASSIGNED_SECTION = new URL(
  "../../../components/manage/gateway/UnassignedSection.svelte",
  import.meta.url,
);
const USER_PROFILES_SECTION = new URL(
  "../../../components/gateway/UserProfilesSection.svelte",
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

// The body of a top-level page function. A guard only earns its pin
// inside the function that must carry it: the same line appears twice
// on the page, so a whole-file match cannot tell one call site from
// the other.
function functionBody(src: string, name: string): string {
  const start = src.indexOf(`function ${name}(`);
  assert(start >= 0, `the page declares ${name}`);
  const end = src.indexOf("\n  }\n", start);
  assert(end > start, `${name} closes at the file's top level`);
  return src.slice(start, end);
}

/** The non-comment lines of a body that touch the unassigned reload. */
function reloadLines(body: string): string[] {
  return body
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line !== "" && !line.startsWith("//"))
    .filter((line) => line.includes("reloadDetailUnassigned("));
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
  "GatewayCatalogPage edits the catalog through the store, not the wire",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    assert(
      src.includes("gatewayStore.refreshAll()"),
      "the read side loads through the store",
    );
    assert(
      src.includes("gatewayStore.saveSet(") && !src.includes("updateSet("),
      "members land as PUTs; description and order as the store's one PATCH",
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
  "GatewayCatalogPage carries the unassigned and credential panels",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    // The panels are section components.
    assert(
      src.includes("<UnassignedSection"),
      "the unassigned panel renders as a section component",
    );
    assert(!src.includes("_pending()"), "no pending placeholder rows render");
    // The unassigned panel rides the store's parked wire filter; a drag
    // rides the same member write as every other placement.
    assert(
      src.includes("gatewayStore.addUnassigned(") &&
        src.includes("gatewayStore.addUnassignedToSet("),
      "unassigned actions ride the store's wraps",
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
      "the credential form is the dialog, not an inline form",
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
        src.includes(
          'names={gatewayStore.setsIn(detailRow?.name ?? "").map((s) => s.name)}',
        ),
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
      src.includes("await this.run(() => listUnassigned())") &&
        !src.includes("unassignedView"),
      "the unassigned read is the single catalog-space list",
    );
    assert(
      src.includes("unassignedBody") && src.includes("listCollectionSets"),
      "unassigned and the nested sets read are store slices, not page-local state",
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
      src.includes("onDropAnywhere") && src.includes("<PlacementDialog"),
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

Deno.test(
  "the remove-collection confirm carries the row's own counts",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    assert(
      src.includes("sets: removeCollectionTarget.set_count") &&
        src.includes("profiles: removeCollectionTarget.profile_count") &&
        src.includes("description={removeCollectionBody}"),
      "the confirm hands over the server's counts",
    );
  },
);

Deno.test("the remove-collection copy renders the counts it is given", () => {
  // The dialog prints whatever the collection view returned, so the
  // numbers reach the user as the server counted them.
  const body = manage_gateway_remove_collection_body({
    name: "alpha",
    sets: 2,
    profiles: 3,
  });
  assert(body.includes("2 profile sets") && body.includes("3 profiles"), body);
});

Deno.test(
  "a profile row is identified by its collection, not by its id alone",
  {
    permissions: {
      read: [PAGE, STORE, UNASSIGNED_SECTION, USER_PROFILES_SECTION],
    },
  },
  () => {
    const page = source(PAGE);
    const store = source(STORE);
    const unassigned = source(UNASSIGNED_SECTION);
    const profiles = source(USER_PROFILES_SECTION);
    // A profile id is unique inside its collection only, and both lists
    // span collections, so the id alone would repeat as an each key.
    assert(
      unassigned.includes("${p.collection_name}/${p.profile_id}"),
      "the unassigned panel keys a row on its collection and id",
    );
    assert(
      profiles.includes("${row.collection_name}/${row.profile_id}"),
      "the user profile list keys a row on its collection and id",
    );
    // The row says which collection, or two same-id rows look alike.
    assert(
      unassigned.includes("{p.collection_name}") &&
        profiles.includes("{row.collection_name}"),
      "both lists print the row's collection",
    );
    // An unassigned profile belongs to one collection, so the detail panel
    // reads only that collection's; the whole-space list is the no-detail case.
    assert(
      page.includes("gatewayStore.unassignedIn(name)") &&
        page.includes("scope={detailRow?.name ?? null}") &&
        page.includes("unassigned={detailRow") &&
        page.includes("(detailUnassigned ?? [])") &&
        page.includes("(gatewayStore.unassigned ?? [])}"),
      "the detail's unassigned panel is narrowed to its collection",
    );
    assert(
      store.includes(
        "listProfiles({ collection_name: collection, parked: true })",
      ),
      "the narrowed read asks the server for that collection",
    );
    // An unassigned profile can only join a set of its own collection, so the
    // drop names the collection and the dialog cannot pick another.
    assert(
      unassigned.includes("onDragStart(p)") &&
        page.includes("dragSourceCollection = profile.collection_name") &&
        page.includes(
          "sourceCollection={placementSourceCollection ?? undefined}",
        ),
      "a drag carries the row's collection out to the placement",
    );
    // With two rows sharing an id, the confirm has to name the row.
    assert(
      page.includes(
        'collection: removeUnassignedTarget?.collection_name ?? ""',
      ),
      "the unassigned delete confirm names its collection",
    );
  },
);

Deno.test(
  "an unassigned reload waits for the drop and the save it follows",
  { permissions: { read: [PAGE] } },
  () => {
    const page = source(PAGE);
    // The reload's own read nulls the store error first, so a reload fired
    // on failure wipes the banner before it paints. It also has to follow
    // the call it reports on: ahead of it, a failure left over from before
    // blocks the reload this edit needs, and a clean verdict fires the
    // reload before this edit lands. Either way the panel keeps old rows.
    const drop = functionBody(page, "onDropToSet");
    const call = "await gatewayStore.addUnassignedToSet(";
    assert(drop.includes(call), `onDropToSet still calls ${call}`);
    // One reload per path is today's convention, not an invariant: a
    // second, conditional reload would be harmless, an unconditional one
    // would not.
    const dropReloads = reloadLines(drop);
    assert(dropReloads.length === 1, "onDropToSet reloads once");
    // The condition is the point: a reload gated on anything but a clean
    // store verdict is the bug this pin keeps out.
    assert(
      dropReloads[0] ===
        "if (gatewayStore.error === null) await reloadDetailUnassigned();",
      "onDropToSet reloads only on a clean verdict",
    );
    assert(
      drop.indexOf("await reloadDetailUnassigned();") > drop.indexOf(call),
      "onDropToSet reloads after the drop lands",
    );
    // A set save is the other way a profile gains or loses a set: the
    // last member leaving flips that profile back to unassigned. The
    // save succeeded here, so this reload runs unconditionally, and it
    // is awaited, so the panel cannot read the store mid-refresh.
    const save = functionBody(page, "onSaveSet");
    const saveReloads = reloadLines(save);
    assert(saveReloads.length === 1, "onSaveSet reloads once");
    assert(
      saveReloads[0] === "await reloadDetailUnassigned();",
      "onSaveSet awaits the reload",
    );
    assert(
      save.indexOf("await reloadDetailUnassigned();") >
        save.indexOf("dialogOpen = false;"),
      "onSaveSet reloads after the dialog closes",
    );
  },
);

Deno.test(
  "a set save settles once, through the sequence's own port",
  { permissions: { read: [PAGE] } },
  () => {
    const page = source(PAGE);
    const save = functionBody(page, "onSaveSet");
    // One save spans several requests: the read side refreshes once, in the
    // sequence, before the re-read that projects what it wrote.
    assert(
      save.includes("settle: () => gatewayStore.refreshAll(),"),
      "the sequence refreshes the read side through its settle port",
    );
    assert(
      save.split("refreshAll").length - 1 === 1,
      "no other refresh rides the save",
    );
    // The order of the two port declarations is only a proxy: what runs
    // first at save time is pinned by saveSet_test's port log.
    assert(
      save.indexOf("settle:") < save.indexOf("reread:"),
      "the refresh lands before the re-read",
    );
  },
);

Deno.test(
  "the page hands the create form the open detail's collection, or null",
  { permissions: { read: [PAGE] } },
  () => {
    const page = source(PAGE);
    // The detail passes its own name; the list entry passes nothing. What
    // the dialog does with either (seeded and locked, or blank) is read in
    // profile_dialog_wiring_test.ts, where the dialog is the subject.
    assert(
      page.includes("pinnedCollection={detailRow?.name ?? null}"),
      "the page hands the detail's collection, or null",
    );
  },
);

Deno.test(
  "the set save rides the lib's step function, not an inline sequence",
  { permissions: { read: [PAGE] } },
  () => {
    const page = source(PAGE);
    // The PUT-then-PATCH sequence and its failure handling are tested
    // directly in lib/manage/gateway/saveSet_test.ts; the page only wires
    // the ports to the store, so the two cannot drift apart.
    assert(
      page.includes("saveSetDraft(target, result, {"),
      "the save runs through the tested step function",
    );
    assert(
      page.includes("gatewayStore.updateMember(") &&
        page.includes("gatewayStore.saveSet(") &&
        !page.includes("failSetSave"),
      "the ports wrap the store, and no page-local failure path remains",
    );
    assert(
      page.includes("landed={setDialogLanded}"),
      "a stopped save hands the dialog the rows that landed",
    );
  },
);
