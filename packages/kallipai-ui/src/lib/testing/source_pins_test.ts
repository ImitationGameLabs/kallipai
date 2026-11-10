import { assert, assertFalse } from "@std/assert";
import { codeLinesOf } from "./source_pins.ts";

Deno.test("a pin reads code lines, never a commented-out copy", () => {
  const dialog = [
    "<p>Don't edit this row.</p>",
    "<script>",
    "  if (!canSubmit) return; // kept, a trailing comment stays",
    "  // if (!canSubmit) return;",
    "  /* const x = parseProfileLimit(raw); */",
    "</script>",
  ].join("\n");
  const code = codeLinesOf(dialog);
  assert(code.includes("if (!canSubmit) return; // kept"));
  assertFalse(code.includes("// if (!canSubmit) return;"));
  assertFalse(code.includes("const x = parseProfileLimit(raw);"));
  // The documented limit: a trailing comment is not stripped, so a shape
  // copied into one still matches. A whole-line comment-out does not.
  assert(
    codeLinesOf("a(); // if (!canSubmit) return;").includes(
      "if (!canSubmit) return;",
    ),
  );
  // The other two: a block or markup comment keeps its inner lines, and a
  // code line that leads with `*` drops out with the comment lines.
  assert(codeLinesOf("/*\nx\n*/").includes("x"));
  assert(codeLinesOf("<!--\nx\n-->").includes("x"));
  assertFalse(codeLinesOf("  * 100;").includes("100;"));
});
