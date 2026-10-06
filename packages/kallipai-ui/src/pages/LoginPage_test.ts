// Source-read pins for the login page's insecure-origin degradation (same
// rationale as message_bubble_test: presentation wiring a typecheck cannot
// see). The passkey ceremony genuinely needs a secure context, so the gate
// moved from disabled controls (a dead input reads as a broken form) to a
// submit-time failure rendered inline through the form's error slot.

import { assert } from "@std/assert";

const PAGE = new URL("./LoginPage.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the online form stays interactive off a secure context",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    assert(
      !src.includes("disabled={!secureContext}"),
      "a disabled username input cannot be focused or typed into, which reads as a broken form",
    );
    assert(
      !src.includes("|| !secureContext"),
      "the submit button enables on a valid username alone",
    );
  },
);

Deno.test(
  "an insecure origin fails at the point of action",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    const guard = src.indexOf("if (!secureContext) {");
    assert(guard >= 0, "the submit path must gate on the secure context");
    assert(
      src.includes("error = login_passkey_insecure();"),
      "the failure reuses the passkey insecure message through the inline error slot",
    );
    // The guard runs after the username validation gate so an invalid
    // handle still reports validation, not the environment.
    assert(
      guard > src.indexOf("if (!canSubmit) return;"),
      "username validation keeps priority over the environment failure",
    );
    assert(
      src.includes("{#if !secureContext && !error}"),
      "the hint yields to the inline error, its own text twice over",
    );
  },
);
