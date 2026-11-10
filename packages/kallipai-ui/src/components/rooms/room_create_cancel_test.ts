import { assert } from "@std/assert";
import { codeLines } from "../../lib/testing/source_pins.ts";

// A create that lands after the user cancelled must not carry them into the
// room they said they did not want, even into a form reopened since. Each
// request carries a number that a cancel bumps, and the dashboard compares
// the two when the result arrives. These pins read the dashboard's source:
// they hold its shape, and a red from them names the line at fault.

const DASHBOARD = new URL("./RoomsDashboard.svelte", import.meta.url);
const PAGE = new URL("../../pages/RoomsPage.svelte", import.meta.url);

Deno.test("the create path numbers its request before it awaits", () => {
  const src = codeLines(DASHBOARD);
  const number = src.indexOf("const mine = ++lastCreate;");
  const awaited = src.indexOf("const id = await onCreate(opts);");
  const success = src.indexOf("if (mine !== lastCreate) return;");
  const close = src.indexOf("createOpen = false;", success + 1);
  const created = src.indexOf("onCreated?.(id);");
  assert(number >= 0, "each create takes a number");
  assert(number < awaited, "the number is taken before the request goes out");
  assert(success > awaited, "the success branch compares before it acts");
  assert(
    close > success && created > close,
    "the form closes before the page is told which room was made",
  );
  assert(
    !src.slice(awaited, created).includes("busy"),
    "the store's busy flag is already cleared here, so it cannot gate the jump",
  );
});

Deno.test("cancel and unmount both forfeit the result", () => {
  const src = codeLines(DASHBOARD);
  const cancel = src.indexOf("onCancel={() => {");
  const close = src.indexOf("createOpen = false;", cancel);
  const bump = src.indexOf("lastCreate += 1;", cancel);
  assert(cancel >= 0, "the dashboard hands the dialog a cancel handler");
  assert(bump > close, "cancelling bumps the number the result compares");
  const cleanup = src.indexOf("$effect(() => () => {");
  const body = src.slice(cleanup, src.indexOf("});", cleanup));
  assert(
    cleanup >= 0 && body.includes("lastCreate += 1;"),
    "leaving the page forfeits it too",
  );
});

Deno.test(
  "the page navigates from the id it was told, not from onCreate",
  () => {
    const src = codeLines(PAGE);
    const from = src.indexOf("onCreate={");
    const to = src.indexOf("onCreated=");
    assert(from >= 0 && to > from, "both props are present");
    assert(
      !src.slice(from, to).includes("navigate("),
      "the create call hands back its id instead of steering the page",
    );
    assert(
      src.slice(to).includes("navigate(`/rooms/${id}`)"),
      "navigation runs from the id the dialog reported",
    );
  },
);

Deno.test("the closed-form failure is logged, not assigned", () => {
  const src = codeLines(DASHBOARD);
  const guard = src.indexOf("if (mine !== lastCreate) {");
  assert(guard >= 0, "the catch compares the same number");
  const block = src.slice(guard, src.indexOf("}", guard));
  assert(
    block.includes(
      'console.error("[rooms] create failed after its form was retired:"',
    ),
    "a failure with no form left is recorded rather than swallowed",
  );
  assert(
    !block.includes("createError ="),
    "a closed form is not handed an error nobody will see",
  );
});
