import { assert, assertEquals } from "@std/assert";
import zhMessages from "../../../i18n/project.inlang/messages/zh/manage_profiles.json" with { type: "json" };
import enMessages from "../../../i18n/project.inlang/messages/en/manage_profiles.json" with { type: "json" };

// Source pins for the profiles page's gateway tab: the tab is the
// connection face (the relay enrollment decides its posture), the
// key UI family is gone with the server face, and the pull
// selection rides the source PUT (the switch request, or the
// unified save committing the same-mode pick) rather than any
// dedicated route or button.
const PAGE = new URL("./ProfilesPage.svelte", import.meta.url);
const STORE = new URL("../../lib/manage/profiles.svelte.ts", import.meta.url);
const TOOLBAR = new URL(
  "../../components/manage/ProfilesToolbar.svelte",
  import.meta.url,
);
const GATEWAY_TAB = new URL(
  "../../lib/manage/profilesGatewayTab.ts",
  import.meta.url,
);
const PREVIEW = new URL(
  "../../components/gateway/GatewayCollectionsPreview.svelte",
  import.meta.url,
);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the gateway tab derives its posture from the serving platform",
  { permissions: { read: [PAGE, GATEWAY_TAB] } },
  () => {
    const src = source(PAGE);
    assert(
      src.includes('shellMode() === "online"'),
      "the posture reads the shell mode",
    );
    assert(
      src.includes("archeionPolisOriginOrFail()"),
      "the switch target is the serving archeion's polis origin",
    );
    assert(
      src.includes("const showGatewayTab = $derived(posture.showGatewayTab);"),
      "the page reads the posture from the pure helper, not inline",
    );
    const lib = source(GATEWAY_TAB);
    assert(
      lib.includes(
        "const showGatewayTab = readOnly || (input.proxyAvailable && input.online);",
      ),
      "the tab exists for the mirror or the online switch entry",
    );
    assert(
      lib.includes("input.sessionReady &&"),
      "the browse face needs a user session",
    );
    assert(
      lib.includes("p.origin === input.servingPolis && p.enrolled"),
      "enrollment matches the serving platform by origin",
    );
  },
);

Deno.test(
  "the local face teaches enrollment instead of a connect flow",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    assert(
      src.includes("{#if !showGatewayTab}"),
      "the local-only shape carries the guidance card",
    );
    assert(
      src.includes("manage_profiles_guide_link()") &&
        src.includes('href="/tagmata"'),
      "the guidance names the tagmata page (the enrollment knob)",
    );
    assert(
      src.includes("manage_profiles_unenrolled_title()") &&
        src.includes("manage_profiles_unenrolled_body()"),
      "an unenrolled platform renders the teaching card, not a switch",
    );
    assert(
      !src.includes("connectTagma") &&
        !src.includes("?connect=") &&
        !src.includes("UserConnectWizard") &&
        !src.includes("manage_profiles_connect_open()"),
      "the connect deep link and wizard family are gone",
    );
  },
);

Deno.test(
  "the switch names the serving platform and carries a touched selection",
  { permissions: { read: [PAGE, STORE] } },
  () => {
    const page = source(PAGE);
    assert(
      page.includes(
        "...(servingPolis !== null ? { polis: servingPolis } : {}),",
      ),
      "the PUT carries the polis whenever the online shell defines it",
    );
    assert(
      page.includes("...(target !== null ? { collection: target } : {}),"),
      "only a resolved pick rides the switch request",
    );
    assert(
      page.includes("selectionWire(gatewayRows(), selection)"),
      "the touched pick rides the request as the wire member",
    );
    const store = source(STORE);
    assert(
      store.includes("polis?: string;") &&
        store.includes("collection?: ProfileSourceSelection;") &&
        store.includes("force?: boolean;"),
      "the store's switch signature carries the source fields and force",
    );
    assert(
      store.includes(
        "...(options.polis !== undefined ? { polis: options.polis } : {}),",
      ),
      "an undefined polis stays off the wire",
    );
  },
);

Deno.test(
  "the connected mirror shows the platform line and rides the unified save",
  { permissions: { read: [PAGE, STORE, TOOLBAR] } },
  () => {
    const page = source(PAGE);
    assert(
      page.includes("manage_profiles_source_platform({") &&
        page.includes("origin: sourceHealth.polis,"),
      "the mirror names the platform it pulls from",
    );
    assert(
      page.includes("pullSelectionDirty(activeHere, selection, derivedKey),"),
      "the touched pick feeds the save-button dirty flag",
    );
    assert(
      page.includes("await onApplySelection().catch(() => {});") &&
        !page.includes("manage_profiles_selection_apply"),
      "the page-level save routes the pick and the dedicated button is gone",
    );
    assert(
      page.includes("pickDirty={selectionDirty}"),
      "the toolbar receives the pick-dirty flag",
    );
    const toolbar = source(TOOLBAR);
    assert(
      toolbar.includes(
        "saveButtonEnabled(readOnly, store.isDirty, pickDirty)",
      ) && toolbar.includes("{#if store.isDirty || pickDirty}"),
      "the save button and the discard affordance both read the pick",
    );
    assert(
      page.includes(
        "{#if servingPlatformEnrolled && sourceHealth?.polis !== servingPolis}",
      ),
      "a connected-elsewhere mirror offers the rebind entry",
    );
    const store = source(STORE);
    assert(
      store.includes('mode: this.config?.source?.mode ?? "local",') &&
        store.includes("collection: target,"),
      "the pick commits through the same-mode PUT over the picked collection",
    );
  },
);

Deno.test(
  "the pull pick derives from the live universe as one radio group",
  { permissions: { read: [PAGE, PREVIEW, GATEWAY_TAB] } },
  () => {
    const lib = source(GATEWAY_TAB);
    assert(
      lib.includes("if (extras < bestExtras) {"),
      "the covering row with the fewest filtered members wins the derived baseline",
    );
    const page = source(PAGE);
    assert(
      page.includes("derivedPullKey("),
      "the page's baseline rides the shared helper",
    );
    const preview = source(PREVIEW);
    assert(
      preview.includes('name="profiles-pull-collection"'),
      "one radio group spans the own rows and the catalog rows",
    );
    assert(
      preview.includes('type="radio"') && !preview.includes("checkbox"),
      "the pick is a native radio, and the checkbox family is gone",
    );
    assert(
      preview.includes('class="radio shrink-0 mt-0.5"'),
      "the pick is a native radio with the form class",
    );
    assert(
      preview.includes(
        "aria-label={manage_profiles_collection_pull({ name: row.name })}",
      ) && preview.includes('name: row.owner + "/" + row.name,'),
      "each row's radio names its collection; catalog rows carry the owner",
    );
    assert(
      preview.includes("onPickRow !== null") &&
        preview.includes("selectionEditable &&"),
      "picks fire only when the page arms the selection",
    );
  },
);

Deno.test(
  "the profiles face carries no key UI or tagma deep link",
  { permissions: { read: [PAGE, STORE] } },
  () => {
    const src = source(PAGE);
    assert(
      !src.includes("UserKey") && !src.includes("user_gateway_key_"),
      "the key family is gone with the server face",
    );
    assert(
      !src.includes("tagmaId"),
      "the page takes no tagma id prop (enrollment replaced the token)",
    );
    assert(
      !source(STORE).includes("listKeys"),
      "the profiles store reads no key list",
    );
  },
);

Deno.test("manage_profiles message keys stay in parity across locales", () => {
  const zh = Object.keys(zhMessages).filter((k) => k !== "$schema");
  const en = Object.keys(enMessages).filter((k) => k !== "$schema");
  assertEquals(zh, en);
});

Deno.test(
  "the gateway source block carries the explicit refetch",
  { permissions: { read: [PAGE, STORE] } },
  () => {
    const src = source(PAGE);
    assert(
      src.includes("void profilesStore.refetchSource()"),
      "the gateway source block offers the refetch action",
    );
    assert(
      src.includes("disabled={profilesStore.isRefetching}"),
      "the refetch button follows the busy flag",
    );
    const store = source(STORE);
    assert(
      store.includes("async refetchSource(): Promise<void>"),
      "the store owns the refetch wiring",
    );
  },
);
