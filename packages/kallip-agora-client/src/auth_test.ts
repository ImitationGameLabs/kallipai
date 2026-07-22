import { assertEquals } from "@std/assert";
import { classifyRegisterConflict } from "./auth.ts";

// `classifyRegisterConflict` is the only code in this package whose
// correctness depends on EXACT string matching against the agora's 409 prose
// (crates/platform/kallip-agora/src/routes/auth.rs: "email already registered" /
// "username already taken"). These tests pin that contract: a server-side
// copy change must update the switch or fail here.

Deno.test("classifyRegisterConflict maps the email-collision message", () => {
  assertEquals(
    classifyRegisterConflict("email already registered"),
    "duplicate-email",
  );
});

Deno.test(
  "classifyRegisterConflict maps the username-collision message",
  () => {
    assertEquals(
      classifyRegisterConflict("username already taken"),
      "duplicate-username",
    );
  },
);

Deno.test("classifyRegisterConflict falls back to unknown on drift", () => {
  // If the agora rephrases the message, surface it as "unknown" (generic
  // failure copy) rather than silently misclassifying -- this is the
  // drift signal.
  assertEquals(classifyRegisterConflict("something else entirely"), "unknown");
});
