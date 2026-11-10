import { assert, assertEquals, assertFalse } from "@std/assert";
import { codeLines } from "../lib/testing/source_pins.ts";

// The confirm shell's two rules that no caller can enforce: a running action
// stays visible, and its key says so. Every caller, the tagma revoke included,
// inherits both. The last two tests pin that caller, which builds its own
// failure copy and tracks which attempt still owns its window.
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

// The revoke is the confirm shell's one caller that builds its own failure
// copy: the shell prints what it is given, so the wrapping and the danger tone
// live at the call site and are pinned there.
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

// A revoke can be cancelled while its request is out (the cancel key stays
// clickable), and its window can be opened again before the reply lands. Two
// guards sort that out: the attempt's number decides whether it still speaks
// for what is on screen, the open flag whether a line is there to write in.
Deno.test("a revoke attempt speaks only for its own window", () => {
  const src = codeLines(CARD);
  before(
    src,
    "const mine = ++revokeSeq;",
    "await onRevoke(",
    "the attempt takes its number before the request goes out",
  );
  assert(
    src.includes("if (mine !== revokeSeq) return;"),
    "a cancelled attempt cannot close a window opened after it",
  );
  assert(
    src.includes("if (mine !== revokeSeq || !confirmingRevoke) {"),
    "a failure is dropped only once both guards agree",
  );
  before(
    src,
    "if (mine !== revokeSeq || !confirmingRevoke) {",
    "revokeError = msgOf(e);",
    "the guard runs before the error slot is written",
  );
  // The bump has to sit in the cancel handler: inside `confirmRevoke` it would
  // abandon every attempt the moment it starts.
  before(
    blockAt(src, "onCancel={() => {"),
    "revokeSeq += 1;",
    "confirmingRevoke = false;",
    "a cancel voids the attempt before it clears the window",
  );
  assertEquals(
    src.split("revokeSeq += 1;").length - 1,
    1,
    "a cancel is the only thing that voids an attempt",
  );
});

/** Source order with both anchors proven present: `indexOf` answers -1 for a
 * missing anchor and -1 sorts before everything, so a bare comparison goes
 * green exactly when the left anchor was deleted. */
function before(src: string, first: string, second: string, message: string) {
  const a = src.indexOf(first);
  const b = src.indexOf(second);
  assert(a >= 0, `anchor missing: ${first}`);
  assert(b >= 0, `anchor missing: ${second}`);
  assert(a < b, message);
}

/** The block that opens at `marker`: from its first `{` to the brace that
 * closes it. Slicing lets a pin speak about what a guard does without
 * copying the sentences it prints. */
function blockAt(src: string, marker: string): string {
  const start = src.indexOf(marker);
  assert(start >= 0, `the marker is there to slice: ${marker}`);
  let depth = 0;
  for (let i = src.indexOf("{", start); i < src.length; i += 1) {
    if (src[i] === "{") depth += 1;
    else if (src[i] === "}") {
      depth -= 1;
      if (depth === 0) return src.slice(start, i + 1);
    }
  }
  throw new Error(`unbalanced block after: ${marker}`);
}

// The guard's whole point is that the reason it drops has a reader. Pinned as
// a shape, not as its sentence, so reworking the wording stays green.
Deno.test("the guard that drops a reason still calls the console", () => {
  const block = blockAt(
    codeLines(CARD),
    "if (mine !== revokeSeq || !confirmingRevoke) {",
  );
  assert(block.includes("console.error("), "the dropped reason is logged");
  assert(block.includes("return;"), "the dropped reason stops the write");
});
