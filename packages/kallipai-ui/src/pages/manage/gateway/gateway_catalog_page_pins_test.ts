import { assert, assertEquals } from "@std/assert";
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
const DETAIL = new URL(
  "../../../lib/manage/gateway/catalogDetail.svelte.ts",
  import.meta.url,
);
const SET_DIALOGS = new URL(
  "../../../lib/manage/gateway/catalogSetDialogs.svelte.ts",
  import.meta.url,
);
const PROVIDER_DIALOGS = new URL(
  "../../../lib/manage/gateway/catalogProviderDialogs.svelte.ts",
  import.meta.url,
);
const PROFILE_DIALOGS = new URL(
  "../../../lib/manage/gateway/catalogProfileDialogs.svelte.ts",
  import.meta.url,
);
const COLLECTION_DIALOGS = new URL(
  "../../../lib/manage/gateway/catalogCollectionDialogs.svelte.ts",
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
    .filter((line) => line.includes("reloadUnassigned("));
}

/** The names a page declares at its top level, in source order: the props,
 * the state, the derived reads, the controllers and any helper. */
function declaredNames(src: string): string[] {
  const names: string[] = [];
  for (const line of src.split("\n")) {
    const destructured = /^ {2}let \{ (\w+)/.exec(line);
    const handler = /^ {2}function (\w+)\(/.exec(line);
    const bound = /^ {2}(?:let|const) (\w+)(?::| =)/.exec(line);
    if (destructured) names.push(destructured[1]);
    else if (handler) names.push(handler[1]);
    else if (bound) names.push(bound[1]);
  }
  return names;
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
  "the console's dialogs edit the catalog through the store, not the wire",
  {
    permissions: {
      read: [
        PAGE,
        SET_DIALOGS,
        PROVIDER_DIALOGS,
        PROFILE_DIALOGS,
        COLLECTION_DIALOGS,
      ],
    },
  },
  () => {
    const page = source(PAGE);
    const sets = source(SET_DIALOGS);
    const providers = source(PROVIDER_DIALOGS);
    const profiles = source(PROFILE_DIALOGS);
    const collections = source(COLLECTION_DIALOGS);
    assert(
      page.includes("gatewayStore.refreshAll()"),
      "the read side loads through the store",
    );
    assert(
      page.includes("store: gatewayStore"),
      "every controller writes through the one store",
    );
    assert(
      sets.includes("store.updateMember(") &&
        sets.includes("store.patchDescription(") &&
        sets.includes("store.reorderSet(") &&
        !page.includes("gatewayStore.saveSet("),
      "members land as PUTs; description and order ride the store's writes",
    );
    assert(
      sets.includes("store.removeSet(") &&
        collections.includes("store.removeCollection("),
      "the remove flows route through the store",
    );
    for (const src of [sets, providers, profiles, collections]) {
      assert(
        !src.includes("adminFetch") && !src.includes("fetch("),
        "no controller speaks the wire directly",
      );
    }
    assert(
      page.includes("<ConfirmDialog"),
      "the destructive remove confirms before it fires",
    );
    assert(
      page.includes("<SetDialog") && page.includes("<SetsSection"),
      "the sets grid and its dialog are the section components",
    );
  },
);

Deno.test(
  "the console carries the unassigned and credential panels",
  {
    permissions: {
      read: [PAGE, SET_DIALOGS, PROVIDER_DIALOGS, PROFILE_DIALOGS],
    },
  },
  () => {
    const page = source(PAGE);
    const sets = source(SET_DIALOGS);
    const providers = source(PROVIDER_DIALOGS);
    const profiles = source(PROFILE_DIALOGS);
    // The panels are section components.
    assert(
      page.includes("<UnassignedSection"),
      "the unassigned panel renders as a section component",
    );
    assert(!page.includes("_pending()"), "no pending placeholder rows render");
    // The unassigned panel rides the store's unassigned wire filter; a drag
    // rides the same member write as every other placement.
    assert(
      profiles.includes("store.addUnassigned(") &&
        profiles.includes("store.addUnassignedToSet("),
      "unassigned actions ride the store's wraps",
    );
    // Credentials: the plaintext crosses inbound only (dialog PUT plus
    // a confirmed delete), through the store wraps.
    assert(
      providers.includes("store.saveCredential(") &&
        providers.includes("store.removeCredential("),
      "credential writes ride the store's credential wraps",
    );
    assert(
      page.includes("<ProviderCredentialDialog"),
      "the credential form is the dialog, not an inline form",
    );
    // The admin URL is deployment-baked: the page never edits it
    // and gates the console on its presence.
    assert(
      !page.includes("gatewayStore.adminUrl") && page.includes("isConnected()"),
      "the admin URL is baked config, never a page-editable field",
    );
    // Set creation is the dialog, landed through the store's add wrap.
    assert(
      sets.includes("store.addSet("),
      "the create-set dialog rides the store's add wrap",
    );
    assert(
      page.includes("<SetCreateDialog") &&
        page.includes("names={setDialogs.create.names}") &&
        sets.includes("store.setsIn("),
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
        DETAIL,
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
      src.includes("placement.dropAnywhere(") &&
        src.includes("<PlacementDialog"),
      "the list view's drop opens the placement fallback dialog",
    );
    assert(
      src.includes("navigate(adminGatewayPath())"),
      "a removed collection's detail returns to the list",
    );
    assert(
      source(DETAIL).includes("readUnassigned(") &&
        src.includes("createCatalogDetail("),
      "the detail's narrowed read and its request token live in one module",
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
  { permissions: { read: [PAGE, COLLECTION_DIALOGS] } },
  () => {
    const page = source(PAGE);
    const dialogs = source(COLLECTION_DIALOGS);
    assert(
      dialogs.includes("sets: target.set_count") &&
        dialogs.includes("profiles: target.profile_count") &&
        page.includes("description={collectionDialogs.collection.remove.body}"),
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
        page.includes("(detail.rows ?? [])") &&
        page.includes("(gatewayStore.unassigned ?? [])}"),
      "the detail's unassigned panel is narrowed to its collection",
    );
    assert(
      store.includes(
        "listProfiles({ collection_name: collection, unassigned: true })",
      ),
      "the narrowed read asks the server for that collection's unassigned",
    );
    // An unassigned profile can only join a set of its own collection, so the
    // drop names the collection and the dialog cannot pick another.
    assert(
      unassigned.includes("onDragStart(p)") &&
        page.includes("placement.startDrag(profile)") &&
        page.includes(
          "sourceCollection={profileDialogs.placement.row?.collection_name ?? undefined}",
        ),
      "a drag carries the row's collection out to the placement",
    );
    // With two rows sharing an id, the confirm has to name the row.
    assert(
      page.includes(
        'collection: profileDialogs.unassigned.remove.target?.collection_name ?? ""',
      ),
      "the unassigned delete confirm names its collection",
    );
  },
);

Deno.test(
  "an unassigned reload waits for the drop and the save it follows",
  { permissions: { read: [PROFILE_DIALOGS, SET_DIALOGS] } },
  () => {
    const profiles = source(PROFILE_DIALOGS);
    const page = source(SET_DIALOGS);
    // The reload's own read nulls the store error first, so a reload fired
    // on failure wipes the banner before it paints. It also has to follow
    // the call it reports on: ahead of it, a failure left over from before
    // blocks the reload this edit needs, and a clean verdict fires the
    // reload before this edit lands. Either way the panel keeps old rows.
    const drop = functionBody(profiles, "onDropToSet");
    const call = "await store.addUnassignedToSet(";
    assert(drop.includes(call), `onDropToSet still calls ${call}`);
    // One reload per path is today's convention, not an invariant: a
    // second, conditional reload would be harmless, an unconditional one
    // would not.
    const dropReloads = reloadLines(drop);
    assert(dropReloads.length === 1, "onDropToSet reloads once");
    // The condition is the point: a reload gated on anything but a clean
    // store verdict is the bug this pin keeps out.
    assert(
      dropReloads[0] === "if (store.error === null) await reloadUnassigned();",
      "onDropToSet reloads only on a clean verdict",
    );
    assert(
      drop.indexOf("await reloadUnassigned();") > drop.indexOf(call),
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
      saveReloads[0] === "await reloadUnassigned();",
      "onSaveSet awaits the reload",
    );
    assert(
      save.indexOf("await reloadUnassigned();") >
        save.indexOf("dialogOpen = false;"),
      "onSaveSet reloads after the dialog closes",
    );
  },
);

Deno.test(
  "a set save settles once, through the sequence's own port",
  { permissions: { read: [SET_DIALOGS] } },
  () => {
    const page = source(SET_DIALOGS);
    const save = functionBody(page, "onSaveSet");
    // One save spans several requests: the read side refreshes once, in the
    // sequence, before the re-read that projects what it wrote.
    assert(
      save.includes("settle: () => store.refreshAll(),"),
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
  { permissions: { read: [PAGE, SET_DIALOGS] } },
  () => {
    const page = source(PAGE);
    const sets = source(SET_DIALOGS);
    // The PUT-then-PATCH sequence and its failure handling are tested
    // directly in lib/gateway/saveSet_test.ts; the page only wires
    // the ports to the store, so the two cannot drift apart.
    assert(
      sets.includes("saveSetDraft(target, result, {"),
      "the save runs through the tested step function",
    );
    assert(
      sets.includes("store.updateMember(") &&
        sets.includes("store.patchDescription(") &&
        sets.includes("store.reorderSet(") &&
        !sets.includes("failSetSave"),
      "each port wraps one store write, and no page-local failure path remains",
    );
    assert(
      page.includes("landed={setDialogs.edit.landed}"),
      "a stopped save hands the dialog the rows that landed",
    );
  },
);

Deno.test(
  "each dialog factory names the interface of what it returns",
  {
    permissions: {
      read: [
        SET_DIALOGS,
        PROVIDER_DIALOGS,
        PROFILE_DIALOGS,
        COLLECTION_DIALOGS,
      ],
    },
  },
  () => {
    // The page is type-checked against these interfaces, so a key set copied
    // into this file would only repeat the compiler. What no copy catches is a
    // factory that stops naming its type, or a controller that starts storing
    // an object literal in $state (null, a flag, a count, a row, an array).
    for (const url of [
      SET_DIALOGS,
      PROVIDER_DIALOGS,
      PROFILE_DIALOGS,
      COLLECTION_DIALOGS,
    ]) {
      const src = source(url);
      assert(
        /export interface \w+Dialogs \{/.test(src),
        `${url.pathname} exports the interface it hands back`,
      );
      assert(
        /\): \w+Dialogs \{/.test(src),
        `${url.pathname} returns that interface`,
      );
      assert(
        !/\$state(<[^>]*>)?\(\{/.test(src),
        `${url.pathname} keeps no object literal in $state`,
      );
    }
  },
);

Deno.test(
  "the page declares the read, the detail and the controllers",
  { permissions: { read: [PAGE] } },
  () => {
    // A field beyond this list is state no controller owns: a dialog's, or
    // the detail read's. The list is the page's whole top level, so a new
    // flag or a handler arrives as a diff here.
    assertEquals(declaredNames(source(PAGE)), [
      "collection",
      "initialLoadDone",
      "detailRow",
      "detailMemberSets",
      "detail",
      "setDialogs",
      "providerDialogs",
      "profileDialogs",
      "collectionDialogs",
      "groupName",
    ]);
  },
);

Deno.test(
  "no controller holds a form draft",
  {
    permissions: {
      read: [
        SET_DIALOGS,
        PROVIDER_DIALOGS,
        PROFILE_DIALOGS,
        COLLECTION_DIALOGS,
      ],
    },
  },
  () => {
    // The dialogs keep their drafts (SetDialog seeds its own form); a
    // controller that stored one would be a second source of truth for
    // what the user typed.
    for (const url of [
      SET_DIALOGS,
      PROVIDER_DIALOGS,
      PROFILE_DIALOGS,
      COLLECTION_DIALOGS,
    ]) {
      const src = source(url);
      assert(
        !/let \w*[Dd]raft\w* = \$state/.test(src),
        `${url.pathname} stores no draft`,
      );
      assert(
        !/\$state<[^>]*Draft/.test(src),
        `${url.pathname} holds no draft state`,
      );
    }
  },
);
