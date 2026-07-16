// Parse a tool-result envelope into a structured shape for rendering.
//
// Every result string the runtime emits is JSON. The standard envelope
// (crates/kallip-runtime/src/policy/executor.rs: success_result/error_result,
// plus approval.rs approval_result_json) is `{"ok":bool,"tool_name":<NAME>,...}`,
// so the tool name is recoverable from the result alone — no toolCall/result
// pairing needed. The four approval_* meta-tools (executor.rs:99-176) emit
// *flat* JSON with no tool_name; those fall through to `generic`.
//
// Pure, no runes, depends only on JSON.

export type ParsedToolResult =
  | { kind: "success"; toolName: string; result: unknown }
  | { kind: "error"; toolName: string; error: string }
  | { kind: "deferred"; toolName: string; id: string; nextSteps: string }
  | { kind: "generic"; data: unknown }
  | { kind: "raw"; text: string };

interface EnvelopeLike {
  ok?: boolean;
  tool_name?: string;
  pending_approval?: boolean;
  id?: string;
  next_steps?: string;
  error?: string;
  result?: unknown;
}

export function parseToolResult(raw: string): ParsedToolResult {
  let data: unknown;
  try {
    data = JSON.parse(raw);
  } catch {
    return { kind: "raw", text: raw };
  }
  if (typeof data !== "object" || data === null) {
    return { kind: "generic", data };
  }
  const env = data as EnvelopeLike;
  // Order matters: the flat approval_* responses are ok:true with no tool_name,
  // so check tool_name before ok/pending_approval.
  if (typeof env.tool_name !== "string") {
    return { kind: "generic", data };
  }
  if (env.pending_approval === true) {
    return {
      kind: "deferred",
      toolName: env.tool_name,
      id: env.id ?? "",
      nextSteps: env.next_steps ?? "",
    };
  }
  if (env.ok === false) {
    return { kind: "error", toolName: env.tool_name, error: env.error ?? "" };
  }
  return { kind: "success", toolName: env.tool_name, result: env.result };
}
