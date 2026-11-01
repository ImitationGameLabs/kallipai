import { assertEquals } from "@std/assert";
import {
  PAT_DEFAULT_PERMS,
  PERM_IDS,
  TAGMA_PERM_IDS,
  permLabel,
  splitPerms,
  toggleStored,
} from "./perms.ts";

// The picker's contract with the server: the ids are wire values, so the
// ordering, the default scope, and the unknown-id handling are pinned here
// rather than left to the components.

Deno.test("the picker and the Rust vocabulary are the same list", async () => {
  const source = await Deno.readTextFile(
    "../../crates/platform/kallipai-archeion-common/src/perm.rs",
  );
  // The constants declare the wire strings; the server rejects anything else,
  // so a drift here is a checkbox the registry would answer 400 for.
  const declared = source
    .split("\n")
    .filter((line) => line.includes("pub const") && line.includes('&str = "'))
    .map((line) => /"([^"]+)"/.exec(line)?.[1])
    .filter((id): id is string => Boolean(id));
  assertEquals(declared, [...PERM_IDS]);
});

Deno.test("the vocabulary keeps the server's picker order", () => {
  assertEquals(PERM_IDS, [
    "archeion:tagma:read",
    "archeion:tagma:write",
    "archeion:account:read",
    "files:record:read",
    "files:record:write",
    "files:delivery:write",
  ]);
});

Deno.test(
  "the tagma picker offers exactly what a tagma can reach",
  async () => {
    const source = await Deno.readTextFile(
      "../../crates/platform/kallipai-archeion-common/src/perm.rs",
    );
    const consts = new Map<string, string>();
    for (const line of source.split("\n")) {
      const m = /pub const ([A-Z][A-Z_]*): &str = "([^"]+)"/.exec(line);
      if (m) consts.set(m[1], m[2]);
    }
    const defaults = source
      .split("\n")
      .find((line) => line.includes("pub const TAGMA_DEFAULTS"))!;
    const named = [...defaults.matchAll(/\b([A-Z][A-Z_]+)\b/g)]
      .map((m) => m[1])
      .filter((name) => consts.has(name))
      .map((name) => consts.get(name)!);
    // A tagma reaches no registry route, so the picker and the migrated set are
    // the file-service strings: a registry checkbox would grant nothing.
    assertEquals(named, [...TAGMA_PERM_IDS]);
    assertEquals(named.length, 3);
  },
);

Deno.test("the personal-token default is reads only", () => {
  assertEquals(PAT_DEFAULT_PERMS, [
    "archeion:tagma:read",
    "archeion:account:read",
    "files:record:read",
  ]);
  for (const p of PAT_DEFAULT_PERMS) {
    assertEquals(PERM_IDS.includes(p), true);
  }
});

Deno.test(
  "toggling adds then removes, and the result stays in vocabulary order",
  () => {
    const added = toggleStored(["files:record:read"], "archeion:tagma:write");
    assertEquals(added, ["archeion:tagma:write", "files:record:read"]);
    assertEquals(toggleStored(added, "archeion:tagma:write"), [
      "files:record:read",
    ]);
  },
);

Deno.test("an emptied set stays empty", () => {
  assertEquals(toggleStored(["files:record:read"], "files:record:read"), []);
});

Deno.test("toggling a stored list keeps unknown strings in place", () => {
  const stored = ["files:record:read", "billing:invoice:read"];
  assertEquals(toggleStored(stored, "files:record:write"), [
    "files:record:read",
    "files:record:write",
    "billing:invoice:read",
  ]);
  assertEquals(toggleStored(stored, "files:record:read"), [
    "billing:invoice:read",
  ]);
});

Deno.test("the tagma picker ids are the file-service subset", () => {
  assertEquals(TAGMA_PERM_IDS, [
    "files:record:read",
    "files:record:write",
    "files:delivery:write",
  ]);
});

Deno.test(
  "splitPerms keeps an unknown id visible instead of dropping it",
  () => {
    const { known, unknown } = splitPerms([
      "files:record:read",
      "billing:invoice:read",
    ]);
    assertEquals(known, ["files:record:read"]);
    assertEquals(unknown, ["billing:invoice:read"]);
  },
);

Deno.test("permLabel names a known id and falls back to the raw id", () => {
  assertEquals(permLabel("files:record:write").length > 0, true);
  assertEquals(permLabel("billing:invoice:read"), "billing:invoice:read");
});
