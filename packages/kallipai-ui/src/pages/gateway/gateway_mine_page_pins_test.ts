import { assert } from "@std/assert";
import { user_gateway_remove_collection_body } from "../../paraglide/messages.js";

// Source pins for the user gateway space: the shell gate owns the
// sign-in posture (any signed-in account qualifies, an admin session
// included), the page rides the UserGatewayStore over the user client,
// renders the reserved audience as the publish dialog's built-in
// option, and touches no storage. The client-side pin enumerates the
// wire paths so the UI consumes exactly the server's route table --
// nothing invented.
const PAGE = new URL("./GatewayMinePage.svelte", import.meta.url);
const STORE = new URL("../../lib/gateway/store.svelte.ts", import.meta.url);
const PROFILES_SECTION = new URL(
  "../../components/gateway/UserProfilesSection.svelte",
  import.meta.url,
);
const DIALOG = new URL(
  "../../components/gateway/UserPublishDialog.svelte",
  import.meta.url,
);
const CREDENTIAL = new URL(
  "../../components/gateway/UserCredentialDialog.svelte",
  import.meta.url,
);
const WEB_ROUTE = new URL(
  "../../../../kallipai-web/src/routes/gateway/+page.svelte",
  import.meta.url,
);
const APP_ROUTE = new URL(
  "../../../../kallipai-app/src/routes/gateway/+page.svelte",
  import.meta.url,
);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

// The body of a top-level page function: a guard earns its pin inside
// the function that must carry it.
function functionBody(src: string, name: string): string {
  const start = src.indexOf(`function ${name}(`);
  assert(start >= 0, `the page declares ${name}`);
  const end = src.indexOf("\n  }\n", start);
  assert(end > start, `${name} closes at the file's top level`);
  return src.slice(start, end);
}

Deno.test(
  "GatewayMinePage leaves the sign-in posture to the shell",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    // The route gate owns sign-in and whoami; the page renders only
    // for a signed-in session and carries none of the old defense
    // branches.
    assert(
      !src.includes("archeionSession"),
      "the page no longer reads the session (the gate owns it)",
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
      !src.includes("user_gateway_sign_in") &&
        !src.includes("user_gateway_user_only"),
      "the dead defense-layer keys are gone",
    );
    assert(
      !src.includes("localStorage") && !src.includes("sessionStorage"),
      "nothing persists beyond the session",
    );
  },
);

Deno.test(
  "GatewayMinePage wires all seven sections and their dialogs through the store",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    for (const section of [
      "<UserProvidersSection",
      "<UserProfilesSection",
      "<UserSetsSection",
      "<UserCollectionsSection",
      "<UserGroupsSection",
    ]) {
      assert(src.includes(section), `${section} must render`);
    }
    for (const dialog of [
      "<UserProviderDialog",
      "<UserCredentialDialog",
      "<UserProfileDialog",
      "<UserSetDialog",
      "<UserCollectionDialog",
      "<UserPublishDialog",
      "<UserGroupDialog",
      "<ConfirmDialog",
    ]) {
      assert(src.includes(dialog), `${dialog} must be mounted`);
    }
    assert(
      src.includes("userGatewayStore.refreshAll()"),
      "the read side loads through the store",
    );
  },
);

Deno.test(
  "the publish dialog offers the reserved audience as a built-in option",
  {
    permissions: { read: [DIALOG] },
  },
  () => {
    const src = source(DIALOG);
    assert(
      src.includes("EVERYONE_GROUP_ID"),
      "the reserved audience rides the shared constant",
    );
    assert(
      src.includes("<option value={EVERYONE_GROUP_ID}>"),
      "the built-in audience is the select's first option",
    );
  },
);

Deno.test(
  "the credential dialog validates the URL with the server's predicate",
  { permissions: { read: [CREDENTIAL] } },
  () => {
    const src = source(CREDENTIAL);
    assert(
      src.includes("isAbsoluteUrl"),
      "the same absolute-URL predicate the server applies",
    );
    assert(
      src.includes("<SecretInput"),
      "the key field never renders as plaintext",
    );
  },
);

Deno.test(
  "UserGatewayStore wraps the user client and keeps no persistent storage",
  { permissions: { read: [STORE] } },
  () => {
    const src = source(STORE);
    assert(
      src.includes('from "./client.ts"'),
      "the store rides the shared user client",
    );
    assert(
      src.includes("bakedGatewayUrl()"),
      "the gateway URL starts from the baked runtime config",
    );
    assert(
      src.includes("export const userGatewayStore = new UserGatewayStore()"),
      "the store is a class singleton (the manage family shape)",
    );
    assert(
      !src.includes("localStorage") && !src.includes("sessionStorage"),
      "nothing persists beyond the session",
    );
  },
);

Deno.test(
  "both shells serve the user space at /gateway",
  { permissions: { read: [WEB_ROUTE, APP_ROUTE] } },
  () => {
    for (const route of [WEB_ROUTE, APP_ROUTE]) {
      assert(
        source(route).includes("GatewayMinePage"),
        "the /gateway route renders the user space",
      );
    }
  },
);

Deno.test(
  "the user space carries no key face or connect deep link",
  { permissions: { read: [PAGE, STORE, WEB_ROUTE, APP_ROUTE] } },
  () => {
    const src = source(PAGE);
    assert(
      !src.includes("UserKey") && !src.includes("keys"),
      "the key UI family is gone (the server dropped the face)",
    );
    assert(
      !src.includes("connectTagma") && !src.includes("?connect="),
      "the connect entry no longer exists",
    );
    assert(!source(STORE).includes("listKeys"), "the store reads no key list");
    for (const route of [WEB_ROUTE, APP_ROUTE]) {
      assert(
        !source(route).includes("connectTagma"),
        "the shells forward no deep link",
      );
    }
  },
);
Deno.test(
  "the sets section groups by collection with a pre-bound create",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    assert(
      src.includes("collections={userGatewayStore.collections ?? []}") &&
        src.includes("onCreate={(collection) => {"),
      "the sets section renders grouped by collection",
    );
    assert(
      src.includes("createCollection = collection;"),
      "the group's create trigger pre-binds the target collection",
    );
    assert(
      src.includes(
        "editing === null ? createCollection : editing.collection_name",
      ) && src.includes("userGatewayStore.addSet(collection, {"),
      "the create lands inside the pre-bound collection",
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
  const body = user_gateway_remove_collection_body({
    name: "alpha",
    sets: 2,
    profiles: 3,
  });
  assert(body.includes("2 profile sets") && body.includes("3 profiles"), body);
});

Deno.test(
  "the user profile list says which collection each row lives in",
  { permissions: { read: [PAGE, PROFILES_SECTION] } },
  () => {
    const page = source(PAGE);
    const section = source(PROFILES_SECTION);
    // The list spans the caller's collections and a profile id repeats
    // across them, so the id alone is not a key.
    assert(
      section.includes("${row.collection_name}/${row.profile_id}"),
      "the list keys a row on its collection and id",
    );
    assert(
      section.includes("{row.collection_name}"),
      "the card prints the row's collection",
    );
    assert(
      page.includes('collection: removeProfileTarget?.collection_name ?? ""'),
      "the delete confirm names the collection the row lives in",
    );
  },
);

Deno.test(
  "a set save settles once, through the sequence's own port",
  { permissions: { read: [PAGE] } },
  () => {
    const page = source(PAGE);
    const body = functionBody(page, "onSaveSet");
    // One save spans several requests: the read side refreshes once, in
    // the sequence, before the re-read projects what it wrote.
    assert(
      body.includes("settle: () => userGatewayStore.refreshAll(),"),
      "the sequence refreshes the read side through its settle port",
    );
    assert(
      body.split("refreshAll").length - 1 === 1,
      "no other refresh rides the save",
    );
    // The order of the two port declarations is only a proxy: what runs
    // first at save time is pinned by saveSet_test's port log.
    assert(
      body.indexOf("settle:") < body.indexOf("reread:"),
      "the refresh lands before the re-read",
    );
  },
);

Deno.test(
  "an edit opens only over rows the page has read",
  { permissions: { read: [PAGE] } },
  () => {
    const page = source(PAGE);
    const body = functionBody(page, "onEditSet");
    // The dialog projects member ids onto the collection's rows: with the
    // rows unread every member would read as deleted, and the first save
    // would clear the set. So the page reads once, then opens only over a
    // detail that could be built.
    assert(
      body.includes("if (userGatewayStore.profiles === null) {") &&
        body.includes("await userGatewayStore.refreshAll();"),
      "unread rows trigger one read before the dialog opens",
    );
    assert(
      body.indexOf("if (detail === null) return;") <
        body.indexOf("editSet = detail;"),
      "no detail, no seed",
    );
    assert(
      body.indexOf("if (detail === null) return;") <
        body.indexOf("setDialogOpen = true;"),
      "no detail, no dialog",
    );
    // The page forbids the silent no-op: the store's reason is on screen.
    assert(
      page.includes("{#if userGatewayStore.error}") &&
        !body.includes("userGatewayStore.error = null"),
      "a refused read keeps its reason on the page alert",
    );
  },
);

Deno.test(
  "a set edit rides the step function and keeps the form on a stop",
  { permissions: { read: [PAGE] } },
  () => {
    const page = source(PAGE);
    // The PUT-then-PATCH sequence and its failure handling are tested in
    // lib/manage/gateway/saveSet_test.ts; the page wires the ports.
    assert(
      page.includes("saveSetDraft(editing, result, {"),
      "the edit save runs through the tested step function",
    );
    assert(
      page.includes("userGatewayStore.updateMember(") &&
        page.includes("userMemberBody(body)") &&
        page.includes("userGatewayStore.saveSet("),
      "the ports wrap the store",
    );
    const body = functionBody(page, "onSaveSet");
    // The member PUT on this face carries the shared four fields only: the
    // served flag belongs to the admin route, so it must not appear in the
    // save's member port.
    const portStart = body.indexOf("putMember:");
    const portEnd = body.indexOf("patchSet:", portStart);
    assert(
      portStart >= 0 && portEnd > portStart,
      "the save wires a member PUT before the set PATCH",
    );
    assert(
      !body.slice(portStart, portEnd).includes("parked"),
      "no served flag rides the user face's PUT",
    );
    assert(
      body.includes("setDialogLanded = outcome.landed;") &&
        body.includes("setDialogError = saveStoppedBanner(outcome);"),
      "a stopped save names what landed and why it stopped",
    );
    // A refusal returns before the close: a closed form has nowhere to
    // paint the server's reason, and the draft is gone with it.
    const stop = body.slice(body.indexOf('if (outcome.phase !== "done")'));
    assert(
      stop.indexOf("return;") < stop.indexOf("setDialogOpen = false;"),
      "a stopped save returns before the dialog closes",
    );
  },
);
