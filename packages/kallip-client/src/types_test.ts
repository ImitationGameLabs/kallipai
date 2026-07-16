import { assertEquals } from "@std/assert";
import {
  sseToDomain,
  wireApprovalToCommon,
  wireStatusToCommon,
} from "./types.ts";
import type { RawSseEvent } from "./types.ts";

Deno.test("sseToDomain renames retrying snake_case fields to camelCase", () => {
  const ev = sseToDomain({
    type: "retrying",
    attempt: 2,
    max_attempts: 3,
    error: "boom",
    delay_secs: 1.5,
  } as RawSseEvent);
  assertEquals(ev, {
    type: "retrying",
    attempt: 2,
    maxAttempts: 3,
    error: "boom",
    delaySecs: 1.5,
  });
});

Deno.test("sseToDomain renames streamReset snake_case fields", () => {
  const ev = sseToDomain({
    type: "streamReset",
    error: "dropped",
    attempt: 1,
    max_attempts: 3,
    delay_secs: 0.5,
  } as RawSseEvent);
  assertEquals(ev, {
    type: "streamReset",
    error: "dropped",
    attempt: 1,
    maxAttempts: 3,
    delaySecs: 0.5,
  });
});

Deno.test(
  "sseToDomain passes approvalUpdated status through (snake_case values)",
  () => {
    const ev = sseToDomain({
      type: "approvalUpdated",
      id: "a1",
      status: "committed",
    });
    assertEquals(ev, {
      type: "approvalUpdated",
      id: "a1",
      status: "committed",
    });
  },
);

Deno.test("sseToDomain keeps single-word fields unchanged", () => {
  assertEquals(sseToDomain({ type: "finished", content: "done" }), {
    type: "finished",
    content: "done",
  });
  assertEquals(
    sseToDomain({ type: "toolCall", name: "bash_exec", args: "{}" }),
    {
      type: "toolCall",
      name: "bash_exec",
      args: "{}",
    },
  );
});

Deno.test("wireApprovalToCommon renames snake_case fields", () => {
  const a = wireApprovalToCommon({
    id: "x",
    requested_by: "agent-1",
    content: { tool_name: "bash_exec", arguments: { cmd: "ls" } },
    commit_reason: "why",
    status: "committed",
    deny_reason: null,
    created_at: "2026-01-01T00:00:00Z",
  });
  assertEquals(a, {
    id: "x",
    requestedBy: "agent-1",
    content: { toolName: "bash_exec", arguments: { cmd: "ls" } },
    commitReason: "why",
    status: "committed",
    denyReason: null,
    createdAt: "2026-01-01T00:00:00Z",
  });
});

Deno.test("wireStatusToCommon renames token_budget / token_consumed", () => {
  const s = wireStatusToCommon({
    state: "busy",
    activity: "thinking",
    token_budget: 1000,
    token_consumed: 42,
  });
  assertEquals(s, {
    state: "busy",
    activity: "thinking",
    tokenBudget: 1000,
    tokenConsumed: 42,
  });
});
