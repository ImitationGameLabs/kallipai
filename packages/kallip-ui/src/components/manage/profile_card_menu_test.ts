import { assert } from "@std/assert";

// Source-read pin for the profile card's kebab menu: every entry must
// live inside <Menu.Content>. An item placed outside the content (a
// direct child of the positioner) sits outside the popup surface the
// menu machinery tracks, so interacting with it never runs the close
// path and the popup stays mounted over the trigger -- blocking the
// next open.
const PROFILE_CARD = new URL("./ProfileCard.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the profile card remove entry renders inside the menu content",
  { permissions: { read: [PROFILE_CARD] } },
  () => {
    const src = source(PROFILE_CARD);
    const remove = src.indexOf('value="remove"');
    assert(remove !== -1, "the remove entry must exist");
    const contentClose = src.indexOf("</Menu.Content>");
    assert(contentClose !== -1, "the menu content must exist");
    assert(
      remove < contentClose,
      "the remove entry must sit inside <Menu.Content>: outside items skip the menu's close path and leave the popup mounted",
    );
  },
);
