import { assert } from "@std/assert";

// Source-read pin for the profile card's kebab menu: the card's remove
// entry must sit inside the ActionMenu, and ActionMenu itself must render
// the items inside <Menu.Content>. An item placed outside the content (a
// direct child of the positioner) sits outside the popup surface the
// menu machinery tracks, so interacting with it never runs the close
// path and the popup stays mounted over the trigger -- blocking the
// next open.
const PROFILE_CARD = new URL("./ProfileCard.svelte", import.meta.url);
const ACTION_MENU = new URL("../ActionMenu.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the profile card remove entry renders inside the shared menu content",
  { permissions: { read: [PROFILE_CARD, ACTION_MENU] } },
  () => {
    const src = source(PROFILE_CARD);
    const remove = src.indexOf('value="remove"');
    assert(remove !== -1, "the remove entry must exist");
    const menuClose = src.indexOf("</ActionMenu>");
    assert(menuClose !== -1, "the shared menu must wrap the card's items");
    assert(
      remove < menuClose,
      "the remove entry must sit inside the shared ActionMenu: outside items skip the menu's close path and leave the popup mounted",
    );
    const shared = source(ACTION_MENU);
    const render = shared.indexOf("{@render children()}");
    const contentClose = shared.indexOf("</Menu.Content>");
    assert(
      render !== -1 && contentClose !== -1,
      "ActionMenu must render the snippet and own a menu content",
    );
    assert(
      render < contentClose,
      "the snippet must render inside <Menu.Content> (the popup close path)",
    );
  },
);
