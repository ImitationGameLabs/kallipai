import { assert, assertFalse } from "@std/assert";
import { codeLines } from "../lib/testing/source_pins.ts";

// The confirm shell's rules that no caller can enforce: a running action
// stays visible, its key says so, and no way out of the window opens while
// the request is out. Every caller, the tagma revoke included, inherits
// them; the last test pins that caller's failure copy and danger tone.
const SRC = new URL("./ConfirmDialog.svelte", import.meta.url);
const CARD = new URL("./tagmata/TagmaCard.svelte", import.meta.url);

Deno.test(
  "the confirm shell keeps a running action from being dismissed",
  () => {
    const src = codeLines(SRC);
    assert(
      src.includes("if (!e.open && !busy) onCancel();"),
      "a backdrop click or Escape while the action runs is ignored",
    );
  },
);

Deno.test("the confirm shell's key reports the running action", () => {
  const src = codeLines(SRC);
  assert(
    src.includes('{busy ? "…" : confirmLabel}'),
    "a running action shows the ellipsis rather than its own label",
  );
});

// A caller builds its own failure copy: the shell prints what it is given, so
// the wrapping and the danger tone live at the call site and are pinned there.
// The tagma revoke is the oldest of them.
Deno.test("the tagma revoke asks through the confirm shell", () => {
  const src = codeLines(CARD);
  assert(src.includes("<ConfirmDialog"), "the revoke question is the shell's");
  assertFalse(
    src.includes("RevokeTagmaDialog"),
    "no copy of the frame survives",
  );
  assert(src.includes('tone="danger"'), "a revoke is drawn as destructive");
  assert(
    src.includes(
      "error={revokeError ? tagma_revoke_failed({ error: revokeError }) : null}",
    ),
    "the server's reason reaches the shell in the revoke's own words",
  );
});

// The window has three ways out: the backdrop, Escape, and the cancel key.
// All three stay shut while the request is out, so a failure that arrives
// late still has a line to land in.
Deno.test("the confirm shell's cancel key shuts while the action runs", () => {
  const src = codeLines(SRC);
  assert(
    src.split("disabled={busy}").length - 1 === 2,
    "no way out of the window is clickable while the action runs",
  );
});
