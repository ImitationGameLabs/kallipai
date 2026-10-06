import { assert } from "@std/assert";

// Source-read pins for the two gateway profile dialogs. The dialogs
// share one contract: an empty or text-only modality selection must
// null out in the payload, a stored provider or effort value must
// stay selectable even when it left the pool, and the tag group
// seeds from the intersection of the stored list with the wire
// modalities.
const USER_DIALOG = new URL("./UserProfileDialog.svelte", import.meta.url);
const MANAGE_DIALOG = new URL(
  "../manage/gateway/ProfileDialog.svelte",
  import.meta.url,
);
const COMPUTE_LIB = new URL("../../lib/manage/compute.ts", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

const NULLING_CHAIN = "normalizeModalities(modalityDraft)?.slice() ?? null";
const KEPT_PROVIDER =
  '{#if providerDraft !== "" && !providers.some((p) => p.provider_id === providerDraft)}';
const KEPT_EFFORT =
  '{#if effortDraft !== "" && !EFFORT_OPTIONS.includes(effortDraft as ReasoningEffort)}';
const INTERSECTION_SEED = "MODALITY_ORDER.filter((m) => stored.includes(m))";

Deno.test(
  "an empty or text-only modality selection nulls out in the payload",
  { permissions: { read: [USER_DIALOG, MANAGE_DIALOG, COMPUTE_LIB] } },
  () => {
    // Behavior: the shared normalizer collapses both to undefined.
    // Chain: the dialogs write "?? null", so the wire body carries
    // null and the server stores its default (never a literal "[]").
    const compute = source(COMPUTE_LIB);
    assert(
      compute.includes('modalities.every((m) => m === "text")'),
      "the normalizer must treat every-text and empty alike",
    );
    for (const url of [USER_DIALOG, MANAGE_DIALOG]) {
      const src = source(url);
      assert(
        src.includes(NULLING_CHAIN),
        "the submit chain must turn the collapsed selection into null",
      );
    }
  },
);

Deno.test(
  "a stored provider or effort value stays selectable in both dialogs",
  { permissions: { read: [USER_DIALOG, MANAGE_DIALOG] } },
  () => {
    for (const url of [USER_DIALOG, MANAGE_DIALOG]) {
      const src = source(url);
      assert(
        src.includes(KEPT_PROVIDER),
        "a provider that left the pool must keep its kept option",
      );
      assert(
        src.includes(KEPT_EFFORT),
        "an effort outside the enum must keep its kept option",
      );
    }
  },
);

Deno.test(
  "the tag group seeds from the stored-versus-wire intersection",
  { permissions: { read: [USER_DIALOG, MANAGE_DIALOG] } },
  () => {
    for (const url of [USER_DIALOG, MANAGE_DIALOG]) {
      const src = source(url);
      assert(
        src.includes("function normalizeModalitiesInit("),
        "the dialog must seed the tag group through the shared shape",
      );
      assert(
        src.includes(INTERSECTION_SEED),
        "seeding must intersect the stored list with the wire modalities",
      );
    }
  },
);

Deno.test(
  "the manage dialog derives the family line from the chosen provider",
  { permissions: { read: [MANAGE_DIALOG] } },
  () => {
    const src = source(MANAGE_DIALOG);
    assert(
      src.includes("familyShown = $derived"),
      "the family line must track the provider through a derived",
    );
  },
);

Deno.test(
  "an empty provider pool shows a create-first hint in create mode",
  { permissions: { read: [USER_DIALOG, MANAGE_DIALOG] } },
  () => {
    for (const url of [USER_DIALOG, MANAGE_DIALOG]) {
      const src = source(url);
      assert(
        src.includes("{#if creating && providers.length === 0}"),
        "the hint must render only when creating with an empty pool",
      );
    }
  },
);
