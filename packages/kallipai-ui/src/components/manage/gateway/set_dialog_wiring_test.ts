import { assert } from "@std/assert";

// Source-read pins for the set dialog's member removal wiring. The card
// must expose a remove action, and the dialog must splice its member
// draft through it. The i18n key itself is verified by the type gate:
// the card imports it from the compiled paraglide modules, and a key
// missing from any locale fails the compile the check task runs on.
const MEMBER_CARD = new URL("./GatewayMemberCard.svelte", import.meta.url);
const SET_DIALOG = new URL("./SetDialog.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the member card carries a remove action wired to an aria label",
  { permissions: { read: [MEMBER_CARD] } },
  () => {
    const src = source(MEMBER_CARD);
    assert(src.includes("onRemove: () => void;"), "the card declares onRemove");
    assert(
      src.includes("aria-label={manage_profiles_set_dialog_remove_member()}"),
      "the remove button's aria label uses the dialog's i18n key",
    );
    assert(
      src.includes("onclick={onRemove}"),
      "the remove button clicks through to onRemove",
    );
  },
);

Deno.test(
  "the dialog splices the member draft on removal and hands it to save",
  { permissions: { read: [SET_DIALOG] } },
  () => {
    const src = source(SET_DIALOG);
    const removeAt = src.indexOf("function removeAt(");
    assert(removeAt !== -1, "the dialog defines removeAt");
    assert(
      src.includes("onRemove={() => removeAt(idx)}"),
      "each member card's onRemove targets its own index",
    );
    // The splice sits inside removeAt, before the draft reassignment.
    assert(
      src.indexOf("next.splice(idx, 1);") > removeAt,
      "removeAt splices the copied list before reassigning the draft",
    );
  },
);
