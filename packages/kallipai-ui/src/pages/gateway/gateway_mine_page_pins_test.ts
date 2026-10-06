import { assert } from "@std/assert";

// Source pins for the user gateway space: the shell gate owns the
// sign-in posture (any signed-in account qualifies, an admin session
// included), the page rides the UserGatewayStore over the user client,
// renders the reserved audience as the publish dialog's built-in
// option, and touches no storage. The client-side pin enumerates the
// wire paths so the UI consumes exactly the server's route table --
// nothing invented.
const PAGE = new URL("./GatewayMinePage.svelte", import.meta.url);
const STORE = new URL("../../lib/gateway/store.svelte.ts", import.meta.url);
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
      src.includes("userGatewayStore.addSet(createCollection, {"),
      "the create lands inside the pre-bound collection",
    );
  },
);
