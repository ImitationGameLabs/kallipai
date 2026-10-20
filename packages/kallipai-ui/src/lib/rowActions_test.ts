// Behaviour tests for the row-interaction controller both transcripts apply:
// one open menu per transcript, the touch hold that opens it, the quote request
// it forwards, the conversation scope that hides a stale menu, the reset a host
// runs when its session moves on or the view goes away, and the hold a late
// timer cannot misplace. Rune-bearing, so it loads behind the
// passthrough `$state` shim reveal_test uses.

import { assertEquals } from "@std/assert";

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { createRowActions } = await import("./rowActions.svelte.ts");

interface Row {
  readonly id: number;
  readonly quotable: boolean;
}

function row(id: number, quotable = true): Row {
  return { id, quotable };
}

function press(pointerType: string) {
  return { pointerType };
}

/** A controller over a mutable scope, so a route change moves it the way a page
 *  swap would. `holdMs` is short so a hold finishes inside a test. */
function setup(initialScope = "room-1") {
  let scope = initialScope;
  const quoted: number[] = [];
  const actions = createRowActions<Row>({
    scope: () => scope,
    idOf: (r) => r.id,
    quotable: (r) => r.quotable,
    onQuote: (r) => quoted.push(r.id),
    holdMs: 5,
  });
  return {
    actions,
    quoted,
    swapScope(next: string) {
      scope = next;
    },
  };
}

Deno.test("opening one row's menu closes the one before it", () => {
  const { actions } = setup();
  const [a, b] = [row(1), row(2)];
  actions.setMenuOpen(a, true);
  assertEquals(actions.isMenuOpen(a), true);
  assertEquals(actions.isMenuOpen(b), false);
  actions.setMenuOpen(b, true);
  assertEquals(actions.isMenuOpen(a), false, "one menu per transcript");
  assertEquals(actions.isMenuOpen(b), true);
});

Deno.test("a close from another row leaves the open one open", () => {
  const { actions } = setup();
  const [a, b] = [row(1), row(2)];
  actions.setMenuOpen(b, true);
  actions.setMenuOpen(a, false);
  assertEquals(actions.isMenuOpen(b), true);
});

Deno.test("closing the row's own menu closes it", () => {
  const { actions } = setup();
  const a = row(1);
  actions.setMenuOpen(a, true);
  actions.setMenuOpen(a, false);
  assertEquals(actions.isMenuOpen(a), false, "the row's own close lands");
});

Deno.test("a touch hold opens that row's menu", async () => {
  const { actions } = setup();
  const a = row(1);
  actions.pressStart(a, press("touch"));
  assertEquals(actions.isMenuOpen(a), false, "nothing opens before the hold");
  await new Promise((resolve) => setTimeout(resolve, 25));
  assertEquals(actions.isMenuOpen(a), true);
});

Deno.test("a mouse press waits out the hold and opens nothing", async () => {
  const { actions } = setup();
  const plain = row(1);
  actions.pressStart(plain, press("mouse"));
  await new Promise((resolve) => setTimeout(resolve, 25));
  assertEquals(actions.isMenuOpen(plain), false, "a mouse press is not a hold");
});

Deno.test("a cancelled hold opens nothing", async () => {
  const { actions } = setup();
  const held = row(2);
  actions.pressStart(held, press("touch"));
  actions.pressCancel();
  await new Promise((resolve) => setTimeout(resolve, 25));
  assertEquals(actions.isMenuOpen(held), false);
});

Deno.test("a row with no affordance ignores the hold", async () => {
  const { actions } = setup();
  const noAffordance = row(1, false);
  actions.pressStart(noAffordance, press("touch"));
  await new Promise((resolve) => setTimeout(resolve, 25));
  assertEquals(actions.isMenuOpen(noAffordance), false);
});

Deno.test("the quote request reaches the page for the row that asked", () => {
  const { actions, quoted } = setup();
  actions.quote(row(7));
  assertEquals(quoted, [7]);
});

Deno.test("a conversation swap drops the open menu", () => {
  const { actions, swapScope } = setup();
  const a = row(1);
  actions.setMenuOpen(a, true);
  swapScope("room-2");
  assertEquals(
    actions.isMenuOpen(a),
    false,
    "a stale id names another room's row",
  );
});

Deno.test(
  "a menu left open stays shut when the host never resets",
  async () => {
    // The reset is early cleanup, not the guard: the entry remembers the
    // conversation it was opened in, so no entry point in the next one can
    // light it up there.
    const { actions, swapScope } = setup();
    const stale = row(1);
    actions.setMenuOpen(stale, true);
    swapScope("room-2");
    assertEquals(actions.isMenuOpen(stale), false, "the swap alone hides it");
    actions.pressStart(stale, press("mouse"));
    assertEquals(
      actions.isMenuOpen(stale),
      false,
      "a mouse press opens nothing",
    );
    actions.pressCancel();
    assertEquals(actions.isMenuOpen(stale), false, "a pointer cancel");
    actions.quote(stale);
    assertEquals(actions.isMenuOpen(stale), false, "a quote");
    actions.pressStart(row(3), press("touch"));
    await new Promise((resolve) => setTimeout(resolve, 25));
    assertEquals(
      actions.isMenuOpen(row(3)),
      true,
      "the hold opens its own row",
    );
    assertEquals(
      actions.isMenuOpen(stale),
      false,
      "and nothing from the room before",
    );
    swapScope("room-1");
    assertEquals(
      actions.isMenuOpen(row(3)),
      false,
      "the new entry stays in its room",
    );

    // The entry's own scope hides a menu from another room; the timer's
    // guard covers the return trip: a hold armed here that comes due while
    // the reader is away must not light the row up once they are back.
    const late = row(4);
    actions.pressStart(late, press("touch"));
    swapScope("room-2");
    await new Promise((resolve) => setTimeout(resolve, 25));
    swapScope("room-1");
    assertEquals(
      actions.isMenuOpen(late),
      false,
      "a hold that came due elsewhere stays shut on the way back",
    );
  },
);

Deno.test("reset drops the open menu and a hold in flight", async () => {
  const { actions } = setup();
  const [a, b] = [row(1), row(2)];
  actions.setMenuOpen(a, true);
  actions.pressStart(b, press("touch"));
  actions.reset();
  await new Promise((resolve) => setTimeout(resolve, 25));
  assertEquals(actions.isMenuOpen(a), false, "a session change drops the menu");
  assertEquals(actions.isMenuOpen(b), false, "and the hold that was in flight");
  actions.setMenuOpen(a, true);
  assertEquals(
    actions.isMenuOpen(a),
    true,
    "the controller still works after the reset",
  );
});

Deno.test(
  "a hold armed in one conversation opens nothing in the next",
  async () => {
    const { actions, swapScope } = setup();
    const stale = row(1);
    actions.pressStart(stale, press("touch"));
    swapScope("room-2");
    await new Promise((resolve) => setTimeout(resolve, 25));
    // An entry point in the new conversation: interacting here must not
    // surface the row the timer was armed for.
    actions.quote(row(2));
    assertEquals(
      actions.isMenuOpen(stale),
      false,
      "the timer only opens the menu in the scope it was armed in",
    );
  },
);

Deno.test("a caller with no session identity keeps one menu", () => {
  // A static caller passes no session key at all, so every read compares
  // `undefined` with `undefined` -- the entry cannot hide itself from the
  // caller that opened it.
  const actions = createRowActions<Row>({
    scope: () => undefined,
    idOf: (r) => r.id,
    quotable: (r) => r.quotable,
  });
  const a = row(1);
  actions.setMenuOpen(a, true);
  assertEquals(
    actions.isMenuOpen(a),
    true,
    "no identity, so nothing hides the menu it opened",
  );
  actions.setMenuOpen(row(2), true);
  assertEquals(actions.isMenuOpen(a), false, "one menu per transcript");
});
