// Unit tests for the summary table: every name it lists is a tool this
// product records, every field it names is one that tool's arguments really
// carry, and anything unlisted or absent yields no line rather than a blank.

import { assert, assertEquals } from "@std/assert";
import { buildJsonTree, type JsonNode } from "./jsonView.ts";
import { summarize, SUMMARY_FIELDS, summaryFieldsFor } from "./toolSummary.ts";

function node(text: string): JsonNode {
  const built = buildJsonTree(text);
  assert(built.ok, `expected a tree for ${text}`);
  return built.node;
}

/** The tool names a 2026-11 corpus of 989 record files carried (658,562
 * calls, 340,944 of them named), most frequent first. A key outside this set
 * would be a typo or a name the product no longer emits -- either way it
 * would never match a record, so the table must not carry it. */
const MEASURED_TOOLS = new Set([
  "bash_exec",
  "break",
  "context_evict",
  "bash_background_read",
  "context_pin",
  "context_unpin",
  "context_status",
  "bash_background_kill",
  "context_pin_last",
  "approval_list",
  "exec_policy",
  "approval_redeem",
  "approval_commit",
  "approval_cancel",
]);

/** One real bash_exec argument string, as the model sent it. */
const BASH_ARGS = node(
  '{"command":"cd /tmp && ls -l dev-reports","timeout":120}',
);

Deno.test("table: every key is a tool the product records", () => {
  for (const name of Object.keys(SUMMARY_FIELDS)) {
    assert(
      MEASURED_TOOLS.has(name),
      `${name} is not a tool this product records`,
    );
    assert(/^[a-z][a-z0-9_]*$/.test(name), `${name} is not a tool name shape`);
  }
});

Deno.test("table: every field is present in a real argument record", () => {
  // The check that catches a typo: a field spelled differently from the wire
  // would find nothing and the summary line would never appear.
  const picked = summarize("bash_exec", BASH_ARGS);
  assertEquals(
    picked.map((p) => p.field),
    [...SUMMARY_FIELDS.bash_exec],
  );
  assert(picked.length > 0);
});

Deno.test("summary: the timeout comes back as its own text", () => {
  assertEquals(summarize("bash_exec", BASH_ARGS), [
    { field: "timeout", value: "120" },
  ]);
  // The command is deliberately absent: the terminal block draws it, from
  // the one place that knows the field, so a summary line would repeat it.
  assertEquals(summarize("bash_exec", node('{"command":"ls"}')), []);
});

Deno.test("summary: an unlisted tool has no fields", () => {
  assertEquals(summaryFieldsFor("break"), []);
  assertEquals(summarize("break", node('{"command":"x"}')), []);
});

Deno.test(
  "summary: a missing, non-scalar or non-object argument is skipped",
  () => {
    assertEquals(summarize("bash_exec", node('{"command":"ls"}')), []);
    assertEquals(summarize("bash_exec", node('{"command":{"a":1}}')), []);
    assertEquals(summarize("bash_exec", node('["command"]')), []);
    assertEquals(summarize("bash_exec", { kind: "null" }), []);
    assertEquals(summarize("bash_exec", null), []);
  },
);

Deno.test("summary: numbers and booleans read as their own text", () => {
  // Only real fields are listed today; the scalar rule is pinned through a
  // table entry this test adds and removes, so it cannot drift unnoticed.
  const saved = SUMMARY_FIELDS.bash_exec;
  SUMMARY_FIELDS.bash_exec = ["timeout", "truncated"];
  try {
    assertEquals(summarize("bash_exec", node('{"timeout":120}')), [
      { field: "timeout", value: "120" },
    ]);
    assertEquals(
      summarize("bash_exec", node('{"timeout":120,"truncated":false}')),
      [
        { field: "timeout", value: "120" },
        {
          field: "truncated",
          value: "false",
        },
      ],
    );
  } finally {
    SUMMARY_FIELDS.bash_exec = saved;
  }
});
