import { assert } from "@std/assert";

// Source-read pins for the set-member profile edit wiring. The card's
// Edit entry must reach a profile-level editor (onEditProfile), not the
// set editor, and the page must route both
// profile contexts through the single shared ProfileDialog.
const SETS_SECTION = new URL("./SetsSection.svelte", import.meta.url);
const PROFILES_PAGE = new URL(
  "../../pages/manage/ProfilesPage.svelte",
  import.meta.url,
);
const PROFILE_DIALOG = new URL("./ProfileDialog.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the set-member card's edit entry targets the profile editor",
  { permissions: { read: [SETS_SECTION] } },
  () => {
    const src = source(SETS_SECTION);
    const wiring = src.indexOf("onEdit={() => onEditProfile(");
    assert(
      wiring !== -1,
      "the card's onEdit must call onEditProfile (a profile-level editor)",
    );
    assert(
      src.indexOf("onEdit={() => onEditSet(") === -1,
      "the card's onEdit must not open the set editor",
    );
  },
);

Deno.test(
  "both profile contexts render through the shared ProfileDialog",
  { permissions: { read: [PROFILES_PAGE] } },
  () => {
    const src = source(PROFILES_PAGE);
    assert(
      src.split("<ProfileDialog").length - 1 === 2,
      "the parking and set-member contexts each mount ProfileDialog once",
    );
    assert(
      src.includes("manage_profiles_profile_dialog_edit_title()"),
      "the set-member dialog speaks with its own i18n keys",
    );
  },
);

Deno.test(
  "text is a permanent, locked modality selection",
  { permissions: { read: [PROFILE_DIALOG] } },
  () => {
    const src = source(PROFILE_DIALOG);
    assert(
      src.includes('m === "text" ||'),
      "the latch must union text into the initial selection",
    );
    assert(
      src.includes('if (m === "text") return;'),
      "toggleModality must refuse to cancel text",
    );
    assert(
      src.includes('{#if m === "text"}'),
      "the text pill must render as a static, non-interactive badge",
    );
  },
);

const PROFILE_CARD = new URL("./ProfileCard.svelte", import.meta.url);

Deno.test(
  "thinking effort rides as absent unless the form touches it",
  { permissions: { read: [PROFILE_DIALOG] } },
  () => {
    const src = source(PROFILE_DIALOG);
    assert(
      src.includes('let effort = $state<ReasoningEffort | "">("");'),
      "a new form starts with no effort selection (empty = not set)",
    );
    assert(
      src.includes('effort = profile?.effort ?? "";'),
      "editing backfills the profile's declared effort",
    );
    assert(
      src.includes("...(effort ? { effort } : {}),"),
      "an untouched or cleared selection rides as absent in the payload",
    );
    assert(
      src.includes("store: profile?.store,"),
      "the store leg rides through untouched",
    );
    assert(
      src.includes("...(modalities ? { modalities } : {}),"),
      "the modalities leg rides as absent unless touched",
    );
  },
);

Deno.test(
  "the profile card shows the effort tier only when it is set",
  { permissions: { read: [PROFILE_CARD] } },
  () => {
    const src = source(PROFILE_CARD);
    assert(
      src.includes("{#if profile.effort}"),
      "the effort row renders only when a tier is declared",
    );
    assert(
      src.includes("manage_profiles_profile_effort_label()"),
      "the card speaks with the effort i18n key",
    );
  },
);

const EFFORT_UNION = new URL(
  "../../../../kallip-client/src/types.ts",
  import.meta.url,
);

Deno.test(
  "the effort select mirrors the ReasoningEffort union",
  { permissions: { read: [PROFILE_DIALOG, EFFORT_UNION] } },
  () => {
    const types = source(EFFORT_UNION);
    const line = types.match(/^export type ReasoningEffort = .*;$/m);
    assert(
      line !== null,
      "the ReasoningEffort union must stay declared in kallip-client",
    );
    const tiers = [...line[0].matchAll(/"([a-z]+)"/g)].map((m) => m[1]);
    assert(tiers.length > 0, "the union must enumerate its members");
    const src = source(PROFILE_DIALOG);
    const start = src.indexOf(
      '<select class="select text-sm" bind:value={effort}>',
    );
    assert(start !== -1, "the dialog must keep an effort select");
    const select = src.slice(start, src.indexOf("</select>", start));
    assert(
      select.includes('<option value="">'),
      "the select must keep an unset option",
    );
    for (const tier of tiers) {
      assert(
        select.includes(`<option value="${tier}">${tier}</option>`),
        `the select must offer the raw tier ${tier}`,
      );
    }
    assert(
      select.split("<option value=").length - 1 === tiers.length + 1,
      "the select offers exactly the union tiers plus unset",
    );
  },
);
