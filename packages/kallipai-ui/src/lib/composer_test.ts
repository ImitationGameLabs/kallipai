// Source-read pins for the composer's submit gating (same rationale as
// attachment_bar_test: stateful runes modules have no runtime test
// harness here, and these gates are exactly what a typecheck cannot
// see). allowEmpty is the hatch the attachment flow uses for file-only
// sends; without it the non-empty draft rule must hold verbatim.

import { assert } from "@std/assert";

const COMPOSER = new URL("./composer.svelte.ts", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "canSend lifts the non-empty draft rule only through allowEmpty",
  { permissions: { read: [COMPOSER] } },
  () => {
    const src = source(COMPOSER);
    assert(
      src.includes(
        "(draft.trim().length > 0 || (options.allowEmpty?.() ?? false))",
      ),
    );
  },
);

Deno.test(
  "submit lifts its empty rejection via allowEmpty, never past canSubmit",
  { permissions: { read: [COMPOSER] } },
  () => {
    const src = source(COMPOSER);
    assert(
      src.includes("(!value && !emptyOk) || !options.canSubmit() || sending"),
    );
  },
);
