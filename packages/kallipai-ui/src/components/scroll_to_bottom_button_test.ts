// Source-read pins for the scroll-to-bottom pill (same rationale as
// message_bubble_test: the contract is presentation wiring a typecheck
// cannot see). The pill is a dumb renderer: the parent owns visibility and
// positioning, and the click hands straight back to the controller.

import { assert } from "@std/assert";

const BUTTON = new URL("./ScrollToBottomButton.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the pill wires the controller contract and the live region",
  { permissions: { read: [BUTTON] } },
  () => {
    const src = source(BUTTON);
    // Outlined at rest, filled on hover (affirmative intent), floating.
    assert(src.includes("preset-outlined-surface-500"));
    assert(src.includes("hover:preset-filled-primary-500"));
    // The badge follows the HubRow unread shape and the 99+ cap helper.
    assert(src.includes("badgeLabel(missed)"));
    assert(src.includes("preset-filled-primary-500"));
    // Count changes announce politely; the visible number stays aria-hidden.
    assert(src.includes(`aria-live="polite"`));
    assert(src.includes("chat_new_while_away_aria"));
    assert(src.includes(`aria-hidden="true"`));
    // The accessible name and tooltip come from the shared chat key.
    assert(src.includes("chat_jump_to_latest()"));
    // The click is the parent-supplied handler.
    assert(src.includes("{onclick}"));
  },
);
