// Source-read pins for the message row's affordances (the discipline of
// offline_view_pins_test: a wiring change that keeps every type-check green can
// still drop the behaviour). Two things are pinned here:
//
//   - the overflow menu is the SHARED ActionMenu, driven by the parent's open
//     flag, so the row carries no menu markup, no dismissal listeners and no
//     focus juggling of its own;
//   - both entries -- the quote button and the menu item -- run the same
//     parent-owned action.

import { assert } from "@std/assert";
import { blockAt } from "../test_support/block.ts";
import { flat } from "../test_support/flat.ts";

const ACTIONS = new URL("./MessageActions.svelte", import.meta.url);
const ACTION_MENU = new URL("./ActionMenu.svelte", import.meta.url);
const VIEW = new URL("./ConversationView.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the row's overflow menu is the shared ActionMenu, parent-controlled",
  () => {
    const src = source(ACTIONS);
    assert(
      src.includes('import ActionMenu from "./ActionMenu.svelte"'),
      "the row must use the shared menu, not its own markup",
    );
    assert(
      src.includes("open={menuOpen}"),
      "the open flag comes from the parent",
    );
    assert(
      src.includes("onOpenChange={onMenuOpenChange}"),
      "dismissal reports back to the parent that owns the flag",
    );
    assert(
      src.includes("triggerClass={BUTTON}"),
      "the trigger keeps the row's own hover affordance styling",
    );
    assert(
      !src.includes('role="menu"') && !src.includes("window.addEventListener"),
      "the menu surface, dismissal and focus are the shared menu's job",
    );
  },
);

Deno.test("both entries run the parent-owned quote action", () => {
  const src = source(ACTIONS);
  assert(
    src.includes("aria-label={chat_quote_action_aria()}"),
    "the direct button keeps its own accessible name",
  );
  assert(src.includes("onclick={() => onQuote()}"), "the direct button quotes");
  assert(
    src.includes('<Menu.Item value="quote"') &&
      src.includes("onSelect={() => onQuote()}"),
    "the overflow item quotes through the menu's select hook",
  );
});

Deno.test("a session change drops the row controller with the scroll", () => {
  const view = flat(source(VIEW));
  const at = view.indexOf("void sessionKey;");
  assert(at !== -1, "the session effect reads the key");
  const effect = blockAt(view, at);
  assert(
    effect.includes("scroll.reset();") &&
      effect.includes("rowActions.reset();") &&
      effect.includes("anchor.drop();"),
    "the host's session effect drops the menu and the up-scroll anchor",
  );
});

Deno.test("the transcript tears the row controller down", () => {
  const view = flat(source(VIEW));
  const at = view.indexOf("$effect(() => () => {");
  assert(at !== -1, "the transcript has a teardown effect");
  const block = blockAt(view, at);
  assert(
    block.includes("reveal.destroy();"),
    "the flash timer stops when the view unmounts",
  );
  assert(
    block.includes("rowActions.reset();"),
    "and the row controller goes with it, not through a session swap",
  );
});

Deno.test("the transcript hands the row controller to the affordance", () => {
  const view = source(VIEW);
  for (const handler of [
    "onpointerdown={(e) => rowActions.pressStart(line, e)}",
    "onpointerup={rowActions.pressCancel}",
    "onpointercancel={rowActions.pressCancel}",
    "onpointerleave={rowActions.pressCancel}",
  ]) {
    assert(view.includes(handler), `touch hold wiring: ${handler}`);
  }
  for (const prop of [
    "menuOpen={rowActions.isMenuOpen(line)}",
    "rowActions.setMenuOpen(line, open)",
    "onQuote={() => rowActions.quote(line)}",
  ]) {
    assert(view.includes(prop), `affordance wiring: ${prop}`);
  }
  assert(
    view.includes("{#if quotable(line)}"),
    "a row the transcript cannot number offers no affordance at all",
  );
  const flatView = flat(source(VIEW));
  const rows = flatView.indexOf(
    "const rowActions = createRowActions<ConversationLine>({",
  );
  assert(rows !== -1, "the row controller is built in the transcript");
  const wiring = flatView.slice(rows, rows + 400);
  for (const line of [
    "idOf: (line) => line.historyId",
    "onQuote: (line) => onQuote?.(line),",
    "scope: () => sessionKey",
  ]) {
    assert(wiring.includes(line), `row controller wiring: ${line}`);
  }
});

Deno.test(
  "the shared menu takes a caller trigger class and a controlled open state",
  () => {
    const src = source(ACTION_MENU);
    assert(src.includes("triggerClass?: string"), "triggerClass is optional");
    assert(src.includes("open?: boolean"), "open is optional");
    assert(
      src.includes("onOpenChange?: (open: boolean) => void"),
      "the callback speaks booleans, not the library's detail object",
    );
    assert(
      src.includes("{...controlled}"),
      "the controlled pair reaches the menu root when the caller owns it",
    );
  },
);
