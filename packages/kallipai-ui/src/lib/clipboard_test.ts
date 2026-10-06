// Source-read pins for the legacy copy path (same rationale as
// message_bubble_test: the contract is browser-DOM wiring a typecheck
// cannot see). Two load-bearing lines back the main-view fake-success
// fix: the mount scope must never come from a global dialog query (the
// shell's agents drawer stays mounted, hidden, while closed, and a
// hidden dialog swallowed the offscreen textarea into an unrendered
// subtree), and a denied focus must fail honestly instead of letting
// Firefox report execCommand("copy") as success on an empty clipboard.

import { assert } from "@std/assert";

const CLIPBOARD = new URL("./clipboard.ts", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the legacy mount scope is the click's own dialog or the body",
  { permissions: { read: [CLIPBOARD] } },
  () => {
    const src = source(CLIPBOARD);
    assert(
      !src.includes("document.querySelector"),
      "a global query can match the closed-but-mounted agents drawer; the scope must come from the interaction, not the page",
    );
    assert(
      src.includes("active.closest(\"[role='dialog'], dialog\")"),
      "an open dialog the click came from is the only dialog scope (its focus trap steals body focus)",
    );
    assert(
      src.includes("?? document.body;"),
      "outside a dialog the textarea mounts on the body",
    );
  },
);

Deno.test(
  "legacy copy fails honestly when focus is denied",
  { permissions: { read: [CLIPBOARD] } },
  () => {
    const src = source(CLIPBOARD);
    assert(
      src.includes("document.activeElement !== area"),
      'without focus there is no live selection; returning anything but false flashes "copied" on an empty clipboard',
    );
  },
);
