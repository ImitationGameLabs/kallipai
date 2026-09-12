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
