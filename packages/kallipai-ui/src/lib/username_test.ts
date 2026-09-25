import { assertEquals } from "@std/assert";
import { isValidUsername } from "./username.ts";

// Mirrors the backend `username.rs` edge-case suite. The frontend helper is a
// pure predicate; the server re-normalizes and is authoritative.

// -- happy paths -----------------------------------------------------------

Deno.test("isValidUsername accepts alphanumeric", () => {
  assertEquals(isValidUsername("alice"), true);
});

Deno.test("isValidUsername accepts single interior hyphens", () => {
  assertEquals(isValidUsername("a-b"), true);
  assertEquals(isValidUsername("a-b-c"), true);
});

Deno.test("isValidUsername accepts all digits", () => {
  assertEquals(isValidUsername("123"), true);
});

Deno.test("isValidUsername accepts boundary lengths", () => {
  assertEquals(isValidUsername("abc"), true);
  assertEquals(isValidUsername("a".repeat(32)), true);
});

Deno.test("isValidUsername trims and lowercases", () => {
  assertEquals(isValidUsername("  Alice-Doe  "), true);
});

// -- hyphen placement ------------------------------------------------------

Deno.test("isValidUsername rejects a leading hyphen", () => {
  assertEquals(isValidUsername("-foo"), false);
});

Deno.test("isValidUsername rejects a trailing hyphen", () => {
  assertEquals(isValidUsername("foo-"), false);
  assertEquals(isValidUsername("a".repeat(31) + "-"), false);
});

Deno.test("isValidUsername rejects consecutive hyphens", () => {
  assertEquals(isValidUsername("foo--bar"), false);
});

Deno.test("isValidUsername rejects hyphen-only inputs", () => {
  assertEquals(isValidUsername("-"), false);
  assertEquals(isValidUsername("---"), false);
});

// -- invalid characters ----------------------------------------------------

Deno.test("isValidUsername rejects underscores", () => {
  assertEquals(isValidUsername("foo_bar"), false);
  assertEquals(isValidUsername("_foo"), false);
});

Deno.test("isValidUsername rejects special chars and non-ASCII", () => {
  assertEquals(isValidUsername("foo@bar"), false);
  assertEquals(isValidUsername("foo.bar"), false);
  assertEquals(isValidUsername("foo bar"), false);
  assertEquals(isValidUsername("café"), false);
});

// -- length bounds ---------------------------------------------------------

Deno.test("isValidUsername rejects underlength", () => {
  assertEquals(isValidUsername("a"), false);
  assertEquals(isValidUsername("ab"), false);
});

Deno.test("isValidUsername rejects overlength", () => {
  assertEquals(isValidUsername("a".repeat(33)), false);
});
