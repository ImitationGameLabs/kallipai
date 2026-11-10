import { assert, assertFalse } from "@std/assert";
import { codeLines } from "../lib/testing/source_pins.ts";

// What each dialog owes the shell. The shell cannot enforce either half from
// inside: a dialog that forgets `busy` still compiles and still renders, and a
// dialog that keeps its own error paragraph renders the message twice. One pin
// over the seven callers keeps that boundary honest.

const DIALOGS = {
  collection: new URL(
    "./manage/gateway/CollectionDialog.svelte",
    import.meta.url,
  ),
  group: new URL("./manage/gateway/GroupDialog.svelte", import.meta.url),
  placement: new URL(
    "./manage/gateway/PlacementDialog.svelte",
    import.meta.url,
  ),
  profile: new URL("./manage/gateway/ProfileDialog.svelte", import.meta.url),
  provider: new URL("./manage/gateway/ProviderDialog.svelte", import.meta.url),
  setCreate: new URL(
    "./manage/gateway/SetCreateDialog.svelte",
    import.meta.url,
  ),
  set: new URL("./manage/gateway/SetDialog.svelte", import.meta.url),
};

for (const [name, url] of Object.entries(DIALOGS)) {
  Deno.test(`the ${name} dialog hands its state to the shell`, () => {
    const src = codeLines(url);
    assertFalse(
      src.includes('role="alert"'),
      "the error line belongs to the shell, so the dialog keeps none of its own",
    );
    const from = src.indexOf("<FormDialog");
    assert(from >= 0, "the dialog renders the shared shell");
    // The opening tag closes on its own line: `\n>` is that line, and a prop
    // expression ending in `>` cannot be mistaken for it.
    const close = src.indexOf("\n>", from);
    assert(close > from, "the opening tag closes");
    const block = src.slice(from, close);
    assert(block.includes("{busy}"), "the shell learns when a save is out");
    assert(block.includes("{error}"), "the shell renders the failure");
    assert(
      block.includes("{canSubmit}") || block.includes("canSubmit={"),
      "the gate reads the caller's verdict rather than a constant",
    );
    assertFalse(
      /canSubmit=\{(true|false)\}/.test(block),
      "a constant handed to the gate passes every signature and guards nothing",
    );
  });
}

Deno.test("the profile dialog's save key carries its number check", () => {
  const src = codeLines(DIALOGS.profile);
  assert(
    src.includes("canSubmit={canSubmit && numbersValid}"),
    "a limit the parser cannot read keeps the save key disabled",
  );
});
