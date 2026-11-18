import { assert } from "@std/assert";
import { codeLines } from "../../testing/source_pins.ts";

// One entry point for the sentence a failed send shows. The unit tests prove
// `sendFailureCopy` itself; only the call sites can prove they go through it,
// and the negative half matters as much: a second path that hands a raw
// `message` to `failInFlight` is exactly the shape this pins out.
const FRAMES = new URL("./frames.ts", import.meta.url);
const OUTBOX = new URL("./outbox.svelte.ts", import.meta.url);

Deno.test(
  "a relay rejection takes its copy from the shared entry point",
  () => {
    const src = codeLines(FRAMES);
    assert(src.includes("sendFailureCopy(reply)"));
    assert(src.includes("failInFlight(localId, failure.copy, failure.code)"));
    assert(!src.includes("failInFlight(localId, reply.message"));
  },
);

Deno.test("a local send failure takes its copy from the entry point", () => {
  const src = codeLines(OUTBOX);
  assert(src.includes("sendFailureCopy(e)"));
  assert(
    src.includes("failInFlight(next.localId, failure.copy, failure.code)"),
  );
  assert(!src.includes("failInFlight(next.localId, e.message"));
});
