import { assert, assertFalse } from "@std/assert";
import { codeLines } from "../lib/testing/source_pins.ts";

// Shape pins for the shell the create/edit dialogs share: one promise per
// test, named in its message. The pinned shape is a source line, because the
// package mounts nothing; a promise whose shape moves is a decision to take
// again, not a refactor.

const SHELL = new URL("./FormDialog.svelte", import.meta.url);

Deno.test("the shell keeps a running save from being dismissed", () => {
  const src = codeLines(SHELL);
  assert(
    src.includes("if (!e.open && !busy) onCancel();"),
    "a backdrop click while a save runs is ignored",
  );
});

Deno.test("the shell widths and the scroll cap are the enumerated ones", () => {
  const src = codeLines(SHELL);
  assert(
    src.includes('const WIDTH = { md: "max-w-md", xl: "max-w-xl" } as const;'),
    "the shell offers exactly the widths the dialogs use",
  );
  assert(
    src.includes('(tall ? " max-h-[85vh] overflow-y-auto" : "")'),
    "the scroll cap is off unless the caller asks for it",
  );
});

Deno.test("the shell's save button reports submitability and busy", () => {
  const src = codeLines(SHELL);
  assert(
    src.includes("disabled={!canSubmit || busy}"),
    "the save button is disabled for both an unsubmittable form and a save in flight",
  );
  assert(
    src.includes("{submitLabel ?? common_save()}"),
    "the save label is the shared one unless the caller varies it",
  );
});

Deno.test("the cancel button stays live while a save runs", () => {
  const src = codeLines(SHELL);
  assert(
    src.includes(
      "<button\n" +
        '              type="button"\n' +
        '              class="btn flex-1 preset-outlined-surface-500 hover:preset-filled-surface-500"\n' +
        "              onclick={onCancel}\n" +
        "            >",
    ),
    "the cancel button opens with its type, class and handler and carries no disabled",
  );
  assert(
    src.includes("{common_cancel()}"),
    "the cancel label is the shared one",
  );
});

Deno.test("the shell refuses a submit while the form cannot submit", () => {
  const src = codeLines(SHELL);
  const gate = src.indexOf("if (!canSubmit || busy) return;");
  const call = src.indexOf("onSubmit();");
  assert(gate >= 0, "the form handler carries the gate");
  assert(gate < call, "the gate stands before the call it guards");
});

Deno.test("the save button sits inside the form", () => {
  const src = codeLines(SHELL);
  const submit = src.indexOf('type="submit"');
  const formEnd = src.indexOf("</form>");
  assert(
    submit >= 0 && formEnd >= 0,
    "both the button and the form close are present",
  );
  assert(submit < formEnd, "the submit button is before the form closes");
});

Deno.test("the error line sits with the fields, not above them", () => {
  const src = codeLines(SHELL);
  const children = src.indexOf("{@render children()}");
  const guard = src.indexOf("{#if error}");
  const footer = src.indexOf('<div class="flex gap-2">');
  assert(
    children >= 0 && guard >= 0 && footer >= 0,
    "all three anchors are present",
  );
  assert(
    children < guard && guard < footer,
    "the error line follows the fields and precedes the footer",
  );
  assert(
    src.indexOf('role="alert"') === src.lastIndexOf('role="alert"'),
    "the shell renders one alert paragraph, so a caller adding its own would show twice",
  );
});

Deno.test("an absent error renders no alert paragraph", () => {
  const src = codeLines(SHELL);
  const open = src.indexOf("{#if error}");
  assert(open >= 0, "the error paragraph sits behind a guard");
  const paragraph = src.indexOf(
    '<p class="text-sm text-error-500 dark:text-error-400" role="alert">',
  );
  assert(paragraph > open, "the paragraph is inside the guard");
  const close = src.indexOf("{/if}", open);
  assert(
    close > paragraph,
    "no closing tag between the guard and the paragraph, so an empty error renders nothing",
  );
  assertFalse(
    src.includes(
      '<p class="text-sm text-error-500 dark:text-error-400" role="alert">{error}',
    ),
    "the paragraph is not emitted outside the guard",
  );
});

Deno.test("the shell nests the dialog's layers in one order", () => {
  const src = codeLines(SHELL);
  const order = [
    "<Dialog {open} {onOpenChange}>",
    "<Portal>",
    "<Dialog.Backdrop",
    "<Dialog.Positioner",
    "<Dialog.Content",
    "<form",
  ].map((shape) => src.indexOf(shape));
  assert(
    order.every((at) => at >= 0),
    `every layer is present: ${order}`,
  );
  assert(
    order.every((at, i) => i === 0 || at > order[i - 1]),
    "each layer opens inside the one before it",
  );
});

Deno.test(
  "the shell's destructive action opens the footer, disabled while saving",
  () => {
    const src = codeLines(SHELL);
    const footer = src.indexOf('<div class="flex gap-2">');
    const destructive = src.indexOf("{#if destructiveAction}");
    const cancel = src.indexOf("onclick={onCancel}");
    assert(
      footer >= 0 && destructive >= 0 && cancel >= 0,
      "all three anchors are present",
    );
    assert(
      footer < destructive && destructive < cancel,
      "the destructive action opens the footer, in front of cancel",
    );
    const close = src.indexOf("{/if}", destructive);
    assert(close > destructive, "the destructive action sits behind a guard");
    const block = src.slice(destructive, close);
    assert(
      block.includes(
        'class="btn flex-1 preset-outlined-error-500 hover:preset-filled-error-500"',
      ),
      "the destructive action wears the preset the shell keeps for it",
    );
    assert(
      block.includes('type="button"'),
      "the button never submits the form the save key submits",
    );
    assert(
      block.includes("disabled={busy}"),
      "a save in flight disables the destructive action too",
    );
    assert(
      block.includes("onclick={destructiveAction.onAction}"),
      "the button runs the action the caller named",
    );
    assert(
      block.includes("{destructiveAction.label}"),
      "the label comes from the caller",
    );
  },
);
