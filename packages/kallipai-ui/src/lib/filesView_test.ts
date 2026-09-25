// Unit tests for the files page view model: prefix stripping, first-segment
// grouping, and the case-insensitive from-the-start filter. Pure
// functions -- no store, no fetch, no DOM.
import { assert, assertEquals } from "@std/assert";
import type { FileEntryView } from "@kallipai/kallipai-files-client";
import { groupFileEntries, matchesFileFilter } from "./filesView.ts";

function entry(id: string, path: string): FileEntryView {
  return { id, path, size: 1, created_at: "2026-09-01T00:00:00Z" };
}

Deno.test(
  "groupFileEntries groups the three named families in server order",
  () => {
    const groups = groupFileEntries(
      [
        entry("1", "/users/u1/inbox/a.pdf"),
        entry("2", "/users/u1/shared/b.txt"),
        entry("3", "/users/u1/tagmas/t7/x.md"),
        entry("4", "/users/u1/inbox/c.txt"),
      ],
      "/users/u1/",
    );
    assertEquals(
      groups.map((g) => [g.kind, g.rows.length]),
      [
        ["inbox", 2],
        ["shared", 1],
        ["tagma", 1],
      ],
    );
    assertEquals(groups[1]!.rows[0]!.displayPath, "shared/b.txt");
  },
);

Deno.test("groupFileEntries resolves the tagma id and splits per tagma", () => {
  const groups = groupFileEntries(
    [
      entry("1", "/users/u1/tagmas/t7/x.md"),
      entry("2", "/users/u1/tagmas/t9/y.md"),
      entry("3", "/users/u1/tagmas/t7/z.md"),
    ],
    "/users/u1/",
  );
  assertEquals(groups.length, 2);
  assertEquals(groups[0]!.tagmaId, "t7");
  assertEquals(groups[0]!.rows.length, 2);
  assertEquals(groups[1]!.tagmaId, "t9");
});

Deno.test("groupFileEntries homes root files and free-form folders", () => {
  const groups = groupFileEntries(
    [
      entry("1", "/users/u1/root-file.bin"),
      entry("2", "/users/u1/docs/y.txt"),
      entry("3", "/users/u1/docs/z.txt"),
    ],
    "/users/u1/",
  );
  assertEquals(
    groups.map((g) => g.kind),
    ["root", "folder"],
  );
  assertEquals(groups[1]!.label, "docs");
  assertEquals(groups[0]!.rows[0]!.displayPath, "root-file.bin");
});

Deno.test(
  "groupFileEntries degrades a foreign prefix to a stripped path",
  () => {
    const groups = groupFileEntries([entry("1", "/other/x.txt")], "/users/u1/");
    assertEquals(groups[0]!.kind, "folder");
    assertEquals(groups[0]!.label, "other");
    assertEquals(groups[0]!.rows[0]!.displayPath, "other/x.txt");
  },
);

Deno.test(
  "matchesFileFilter matches from the start, case-insensitively",
  () => {
    assert(matchesFileFilter("report.pdf", ""));
    assert(matchesFileFilter("report.pdf", "REP"));
    assert(matchesFileFilter("shared/report.pdf", "  shared/re"));
    // Not a substring search: the match anchors at the path head.
    assertEquals(matchesFileFilter("shared/report.pdf", "report"), false);
    assertEquals(matchesFileFilter("report.pdf", "port"), false);
  },
);
