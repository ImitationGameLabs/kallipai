import { assert } from "@std/assert";

// Source pins for the parking placement fallback: the dialog owns a
// two-step pick (collection, then member set), the cascade narrows
// the set list through the shared membership helper, a collection
// change resets the set pick instead of waiting for a blur, the
// parked profile's identity rides the dialog head, and the confirm
// carries both picks to the caller.
const DIALOG = new URL("./ParkedPlacementDialog.svelte", import.meta.url);
const HELPER = new URL("../../../lib/gateway/collections.ts", import.meta.url);
const USER_SECTION = new URL(
  "../../gateway/UserSetsSection.svelte",
  import.meta.url,
);
const CATALOG_PAGE = new URL(
  "../../../pages/manage/gateway/GatewayCatalogPage.svelte",
  import.meta.url,
);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the placement dialog cascades, resets on change, and confirms a pair",
  { permissions: { read: [DIALOG] } },
  () => {
    const src = source(DIALOG);
    assert(
      src.includes("memberSetsOf("),
      "the member-set cascade rides the shared helper",
    );
    assert(
      src.includes('onchange={() => (setDraft = "")}') &&
        !src.includes("onblur={() => (setDraft"),
      "a collection change resets the set pick without waiting for blur",
    );
    assert(
      src.includes("disabled={busy || memberSets.length === 0}"),
      "a collection without sets disables the set pick",
    );
    assert(
      src.includes("onConfirm(collectionDraft, setDraft)"),
      "the confirm carries both picks to the caller",
    );
    assert(
      src.includes(
        "manage_gateway_placement_profile_label({ name: profileId })",
      ),
      "the dialog head names the parked profile being placed",
    );
  },
);

Deno.test(
  "the member-set filter is single-sourced across both faces",
  { permissions: { read: [HELPER, DIALOG, USER_SECTION, CATALOG_PAGE] } },
  () => {
    const helper = source(HELPER);
    assert(
      helper.includes("export function memberSetsOf"),
      "the helper owns the membership filter",
    );
    for (const [name, url] of [
      ["the placement dialog", DIALOG],
      ["the caller's grouped sets", USER_SECTION],
      ["the console detail", CATALOG_PAGE],
    ] as const) {
      assert(
        source(url).includes("memberSetsOf("),
        `${name} rides the shared helper`,
      );
    }
  },
);
