import { assertEquals } from "@std/assert";
import { isValidEmail } from "./email.ts";

Deno.test("isValidEmail accepts a canonical address", () => {
  assertEquals(isValidEmail("alice@example.com"), true);
});

Deno.test("isValidEmail accepts plus-addressing and dots", () => {
  assertEquals(isValidEmail("user.name+tag@example.org"), true);
});

Deno.test("isValidEmail accepts mixed case (does not canonicalize)", () => {
  // email.rs treats the local part as case-sensitive -- the helper must not
  // lowercase, so a mixed-case address is still a valid shape.
  assertEquals(isValidEmail("John@Example.COM"), true);
});

Deno.test("isValidEmail rejects a missing @", () => {
  assertEquals(isValidEmail("not-an-email"), false);
});

Deno.test("isValidEmail rejects an empty local part", () => {
  assertEquals(isValidEmail("@example.com"), false);
});

Deno.test("isValidEmail rejects an interior space", () => {
  assertEquals(isValidEmail("a @b.com"), false);
  assertEquals(isValidEmail("a@b .com"), false);
});

Deno.test("isValidEmail rejects an address over 254 octets", () => {
  const local = "a".repeat(300);
  assertEquals(isValidEmail(`${local}@x.com`), false);
});

Deno.test("isValidEmail trims surrounding whitespace", () => {
  assertEquals(isValidEmail("  alice@example.com  "), true);
});
