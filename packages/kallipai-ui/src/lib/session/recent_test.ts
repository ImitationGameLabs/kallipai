// Tests for the recency ledger and the ordering derived from it. Everything
// here is pure: keys come in from the caller, so no store or Svelte compiler
// is involved.

import { assertEquals, assertNotStrictEquals } from "@std/assert";
import {
  LEDGER_CAP,
  RECENT_CAP,
  capRecent,
  noteActivity,
  parseLedger,
  pruneLedger,
  recentConversations,
} from "./recent.ts";

Deno.test("a later stamp wins and an earlier one is ignored", () => {
  const once = noteActivity({}, "room:a", 100);
  assertEquals(once, { "room:a": 100 });
  assertEquals(noteActivity(once, "room:a", 300), { "room:a": 300 });
  // The envelope sink and the page tick report the same room independently;
  // whichever lands second must not drag the row backwards.
  assertEquals(noteActivity(once, "room:a", 40), { "room:a": 100 });
});

Deno.test("stamping one key leaves the others alone", () => {
  const ledger = { "room:a": 10, "tagma:b": 20 };
  assertEquals(noteActivity(ledger, "room:c", 30), {
    "room:a": 10,
    "tagma:b": 20,
    "room:c": 30,
  });
  assertEquals(ledger, { "room:a": 10, "tagma:b": 20 });
});

Deno.test("unreadable stored ledgers degrade to empty", () => {
  assertEquals(parseLedger(null), {});
  assertEquals(parseLedger("not json"), {});
  assertEquals(parseLedger("[1,2]"), {});
  assertEquals(parseLedger('"a string"'), {});
  assertEquals(parseLedger("null"), {});
});

Deno.test("parsing drops values that are not finite numbers", () => {
  assertEquals(
    parseLedger('{"room:a": 5, "room:b": "6", "room:c": null, "room:d": 7.5}'),
    { "room:a": 5, "room:d": 7.5 },
  );
  // Zero is a real stamp (epoch), not a missing one.
  assertEquals(parseLedger('{"room:a": 0}'), { "room:a": 0 });
});

Deno.test("pruning keeps the newest entries and drops the rest", () => {
  const ledger: Record<string, number> = {};
  for (let i = 0; i < LEDGER_CAP + 5; i++) ledger[`room:${i}`] = 1000 + i;
  const pruned = pruneLedger(ledger);
  assertEquals(Object.keys(pruned).length, LEDGER_CAP);
  assertEquals(pruned[`room:${LEDGER_CAP + 4}`], 1000 + LEDGER_CAP + 4);
  assertEquals(pruned["room:0"], undefined);
  assertEquals(pruned["room:4"], undefined);
  assertEquals(pruned["room:5"], 1005);
});

Deno.test("pruning equal stamps keeps a stable literal order", () => {
  const ledger: Record<string, number> = {};
  for (let i = 0; i < LEDGER_CAP + 2; i++) ledger[`room:${i}`] = 1000;
  const pruned = pruneLedger(ledger);
  assertEquals(Object.keys(pruned).length, LEDGER_CAP);
  // The same input twice, and the survivors are the literal-first keys:
  // room:0, room:1, room:10 -- not an order the browser's language picks.
  assertEquals(Object.keys(pruneLedger(ledger)), Object.keys(pruned));
  assertEquals(Object.keys(pruned).slice(0, 3), [
    "room:0",
    "room:1",
    "room:10",
  ]);
});

Deno.test(
  "pruning a small ledger returns a copy that does not alias it",
  () => {
    const ledger = { "room:a": 3, "room:b": 1 };
    const pruned = pruneLedger(ledger);
    assertEquals(pruned, ledger);
    assertNotStrictEquals(pruned, ledger);
    ledger["room:a"] = 99;
    assertEquals(pruned["room:a"], 3);
  },
);

Deno.test("rows with a stamp come first, newest first", () => {
  const rows = [{ key: "room:a" }, { key: "room:b" }, { key: "room:c" }];
  const ordered = recentConversations(rows, {
    "room:a": 100,
    "room:b": 300,
    "room:c": 200,
  });
  assertEquals(
    ordered.map((row) => row.key),
    ["room:b", "room:c", "room:a"],
  );
});

Deno.test("a stamp of zero counts as stamped, not as missing", () => {
  // Epoch is a real stamp. A truthiness check here would drop the row into
  // the untimed group, where the unread one outranks it.
  const rows = [{ key: "room:fresh", unread: 3 }, { key: "room:epoch" }];
  const ordered = recentConversations(rows, { "room:epoch": 0 });
  assertEquals(
    ordered.map((row) => row.key),
    ["room:epoch", "room:fresh"],
  );
});

Deno.test("rows without a stamp keep registration order, unread first", () => {
  const rows = [
    { key: "room:a" },
    { key: "room:b", unread: 2 },
    { key: "room:c" },
    { key: "room:d", unread: 1 },
  ];
  const ordered = recentConversations(rows, {});
  assertEquals(
    ordered.map((row) => row.key),
    ["room:b", "room:d", "room:a", "room:c"],
  );
});

Deno.test("a stamped row outranks an unread one it never touched", () => {
  const rows = [{ key: "room:fresh", unread: 9 }, { key: "room:visited" }];
  const ordered = recentConversations(rows, { "room:visited": 10 });
  assertEquals(
    ordered.map((row) => row.key),
    ["room:visited", "room:fresh"],
  );
});

Deno.test("a stamped row sorts by time even when it has unread replies", () => {
  const rows = [
    { key: "room:a", unread: 5 },
    { key: "room:b", unread: 0 },
  ];
  const ordered = recentConversations(rows, {
    "room:a": 100,
    "room:b": 200,
  });
  assertEquals(
    ordered.map((row) => row.key),
    ["room:b", "room:a"],
  );
});

Deno.test("ledger entries whose conversation is gone are not listed", () => {
  const rows = [{ key: "room:a" }];
  const ordered = recentConversations(rows, {
    "room:a": 100,
    "room:left": 900,
  });
  assertEquals(
    ordered.map((row) => row.key),
    ["room:a"],
  );
});

Deno.test("the cap cuts the tail and keeps the order", () => {
  const rows = Array.from({ length: RECENT_CAP + 1 }, (_, i) => ({
    key: `room:${i}`,
  }));
  const capped = capRecent(rows);
  assertEquals(capped.length, RECENT_CAP);
  assertEquals(
    capped.map((row) => row.key),
    rows.slice(0, RECENT_CAP).map((row) => row.key),
  );
});

Deno.test("a list at the cap is returned whole", () => {
  const rows = Array.from({ length: RECENT_CAP }, (_, i) => ({
    key: `room:${i}`,
  }));
  assertEquals(capRecent(rows).length, rows.length);
});
