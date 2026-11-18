// Pins, not documentation: the wire shape of every verb TagmaClient exposes,
// as recorded here. Each row was authored from docs/en/reference/tagma-api.md
// and the tagma's route table (crates/kallipai-tagma/src/routes*.rs, plus
// the request structs), not from this package's source.
//
// The table is a snapshot, not a two-end reconciliation: the doc and the
// Rust side can move without turning this red.
//
// externalEventStream is a stream, not a method answer: its row pins the
// request it opens, and each frame is `{event, data}` — the caller decodes
// a payload per its event name, so no single return type describes the verb.
//
// A row's `query` and `body` lists are the keys that travel, sorted; an
// empty list means the verb sends none. A row records what this client
// sends, which can be narrower than what the server accepts. For example,
// `postMessage` leaves `defer` at its default, and `closeTask` cannot carry
// `dossier_tar_b64` at all, a capability gap rather than a default-value
// difference. Path-parameter escaping is outside the pin too: dropping an
// `encodeURIComponent` would not turn it red.
//
// Each row's answer type rides a `satisfies`, which binds the type name:
// a change inside types.ts stays invisible here.

import { assert, assertEquals } from "@std/assert";
import type { FileAttachment } from "@kallipai/kallipai-common";
import { withFetch } from "@kallipai/kallipai-common/testing";
import { TagmaClient } from "./client.ts";
import type {
  AgentStatusResponse,
  BudgetResponse,
  CallBody,
  DeleteSetResponse,
  ExternalHistoryResponse,
  ListAgentsManagementResponse,
  MessageBody,
  MessageResponse,
  ProfileApplyResponse,
  ProfileConfig,
  ProfileProbeResponse,
  TaskExport,
  TaskListPage,
  TurnDetail,
  TurnsPage,
  WireAgentSummary,
  WorkSchedule,
} from "./types.ts";

type Dial = (client: TagmaClient) => Promise<unknown>;

interface Row {
  readonly name: string;
  readonly method: string;
  readonly path: string;
  readonly query: readonly string[];
  readonly body: readonly string[];
  readonly dial: Dial;
}

/** What a stub answer looks like: the stream shape for the SSE verb, an
 * empty JSON object everywhere else. The verbs parse their own answers, and a
 * parse that fails on `{}` is irrelevant here — the request is already
 * recorded by then. */
function answer(url: string): Promise<Response> {
  if (url.includes("/events")) {
    const body = new ReadableStream<Uint8Array>({
      start: (controller) => controller.close(),
    });
    return Promise.resolve(
      new Response(body, {
        status: 200,
        headers: { "content-type": "text/event-stream" },
      }),
    );
  }
  return Promise.resolve(
    new Response("{}", {
      status: 200,
      headers: { "content-type": "application/json" },
    }),
  );
}

/** The keys of the JSON body the seam put on the wire, none when the verb sent
 * no body. Any other body form is a shape this pin cannot read, so it fails
 * instead of answering as if the verb had sent nothing. */
function sentBodyKeys(init?: RequestInit): readonly string[] {
  const body = init?.body;
  if (body === undefined || body === null) return [];
  assert(typeof body === "string", "the wire body is no longer JSON text");
  const parsed = JSON.parse(body) as Record<string, unknown>;
  return Object.keys(parsed).sort();
}

/** The agent, budget, profile and schedule surface. */
const AGENT_ROWS: readonly Row[] = [
  {
    name: "postMessage",
    method: "POST",
    path: "/agents/a1/message",
    query: [],
    body: ["attachment", "quote", "text"],
    dial: (c) =>
      c.postMessage(
        "a1",
        "hi",
        {} as FileAttachment,
        7,
      ) satisfies Promise<MessageResponse>,
  },
  {
    name: "getRootAgent",
    method: "GET",
    path: "/agents/root",
    query: [],
    body: [],
    dial: (c) => c.getRootAgent() satisfies Promise<WireAgentSummary>,
  },
  {
    name: "externalEventStream",
    method: "GET",
    path: "/agents/a1/external/events",
    query: [],
    body: [],
    dial: async (c) => {
      const frames = c.externalEventStream("a1") satisfies AsyncGenerator<{
        readonly event: string;
        readonly data: string;
      }>;
      await frames.next();
      await frames.return(undefined);
    },
  },
  {
    name: "externalHistory",
    method: "GET",
    path: "/agents/a1/external/history",
    query: ["after_seq", "before_seq", "limit", "row"],
    body: [],
    dial: (c) =>
      c.externalHistory("a1", {
        afterSeq: 1,
        beforeSeq: 2,
        row: 3,
        limit: 4,
      }) satisfies Promise<ExternalHistoryResponse>,
  },
  {
    name: "getBudget",
    method: "GET",
    path: "/budget",
    query: [],
    body: [],
    dial: (c) => c.getBudget() satisfies Promise<BudgetResponse>,
  },
  {
    name: "updateBudget",
    method: "POST",
    path: "/budget",
    query: [],
    body: ["delta", "set_remaining", "set_unlimited"],
    dial: (c) =>
      c.updateBudget({
        set_remaining: 1,
        delta: 2,
        set_unlimited: true,
      }) satisfies Promise<BudgetResponse>,
  },
  {
    name: "listAgents",
    method: "GET",
    path: "/agents",
    query: ["created_by"],
    body: [],
    dial: (c) =>
      c.listAgents({
        created_by: "a1",
      }) satisfies Promise<ListAgentsManagementResponse>,
  },
  {
    name: "getAgentStatus",
    method: "GET",
    path: "/agents/a1/status",
    query: [],
    body: [],
    dial: (c) => c.getAgentStatus("a1") satisfies Promise<AgentStatusResponse>,
  },
  {
    name: "interruptAgent",
    method: "POST",
    path: "/agents/a1/interrupt",
    query: [],
    body: [],
    dial: (c) => c.interruptAgent("a1") satisfies Promise<void>,
  },
  {
    name: "removeAgent",
    method: "DELETE",
    path: "/agents/a1",
    query: [],
    body: [],
    dial: (c) => c.removeAgent("a1") satisfies Promise<void>,
  },
  {
    name: "setAgentDuty",
    method: "PUT",
    path: "/agents/a1/duty",
    query: [],
    body: ["status"],
    dial: (c) =>
      c.setAgentDuty("a1", { status: "onduty" }) satisfies Promise<void>,
  },
  {
    name: "updateAgentMetadata",
    method: "PUT",
    path: "/agents/a1/metadata",
    query: [],
    body: ["description", "role"],
    dial: (c) =>
      c.updateAgentMetadata("a1", {
        role: "r",
        description: "d",
      }) satisfies Promise<void>,
  },
  {
    name: "getProfiles",
    method: "GET",
    path: "/profiles",
    query: [],
    body: [],
    dial: (c) => c.getProfiles() satisfies Promise<ProfileConfig>,
  },
  {
    name: "updateProfiles",
    method: "PUT",
    path: "/profiles",
    query: [],
    body: ["default", "endpoints", "force", "parking", "sets", "source"],
    dial: (c) =>
      c.updateProfiles({
        sets: [],
        default: "s1",
        endpoints: {},
        parking: [],
        force: true,
        source: { mode: "local" },
      }) satisfies Promise<ProfileConfig>,
  },
  {
    name: "applyProfiles",
    method: "POST",
    path: "/profiles/apply",
    query: [],
    body: [],
    dial: (c) => c.applyProfiles() satisfies Promise<ProfileApplyResponse>,
  },
  {
    name: "refreshProfiles",
    method: "POST",
    path: "/profiles/refresh",
    query: [],
    body: [],
    dial: (c) => c.refreshProfiles() satisfies Promise<ProfileConfig>,
  },
  {
    name: "probeProfiles",
    method: "POST",
    path: "/profiles/probe",
    query: [],
    body: ["endpoints", "sets"],
    dial: (c) =>
      c.probeProfiles({
        endpoints: [],
        sets: [],
      }) satisfies Promise<ProfileProbeResponse>,
  },
  {
    name: "deleteProfileSet",
    method: "DELETE",
    path: "/profiles/sets/s1",
    query: ["force"],
    body: [],
    dial: (c) =>
      c.deleteProfileSet("s1", true) satisfies Promise<DeleteSetResponse>,
  },
  {
    name: "getWorkSchedule",
    method: "GET",
    path: "/work-schedule",
    query: [],
    body: [],
    dial: (c) => c.getWorkSchedule() satisfies Promise<WorkSchedule>,
  },
  {
    name: "putWorkSchedule",
    method: "PUT",
    path: "/work-schedule",
    query: [],
    body: [
      "final_warn_minutes",
      "final_warn_prompt",
      "pre_warn_minutes",
      "spec",
      "status",
      "wake_prompt",
    ],
    dial: (c) =>
      c.putWorkSchedule({
        spec: { mode: "always" },
        pre_warn_minutes: 1,
        final_warn_minutes: 2,
        wake_prompt: "",
        final_warn_prompt: "",
        status: "active",
      }) satisfies Promise<WorkSchedule>,
  },
];

/**
 * The task ledger: list and read verbs, write verbs, then the archive and
 * export verbs.
 */
const TASK_ROWS: readonly Row[] = [
  {
    name: "listTasks",
    method: "GET",
    path: "/tasks",
    query: [
      "archived",
      "assignee",
      "limit",
      "offset",
      "since",
      "status",
      "time",
      "until",
    ],
    body: [],
    dial: (c) =>
      c.listTasks({
        status: "queued",
        assignee: "a1",
        archived: false,
        time: "updated",
        since: 1,
        until: 2,
        limit: 3,
        offset: 4,
      }) satisfies Promise<TaskListPage>,
  },
  {
    name: "getTask",
    method: "GET",
    path: "/tasks/7",
    query: [],
    body: [],
    dial: (c) => c.getTask(7) satisfies Promise<TaskExport>,
  },
  {
    name: "createTask",
    method: "POST",
    path: "/tasks",
    query: [],
    body: [
      "assignee",
      "inbox_id_end",
      "inbox_id_start",
      "require",
      "room_id",
      "room_seq_end",
      "room_seq_start",
      "title",
    ],
    dial: (c) =>
      c.createTask({
        title: "t",
        assignee: "a1",
        require: ["a1"],
        inbox_id_start: 1,
        inbox_id_end: 2,
        room_id: "r1",
        room_seq_start: 3,
        room_seq_end: 4,
      }) satisfies Promise<TaskExport>,
  },
  {
    name: "startTask",
    method: "POST",
    path: "/tasks/7/start",
    query: [],
    body: ["force"],
    dial: (c) => c.startTask(7, { force: true }) satisfies Promise<TaskExport>,
  },
  {
    name: "confirmTask",
    method: "POST",
    path: "/tasks/7/confirm",
    query: [],
    body: ["file", "note"],
    dial: (c) =>
      c.confirmTask(7, {
        note: "n",
        file: { file_name: "f.md", file_b64: "AA" },
      }) satisfies Promise<TaskExport>,
  },
  {
    name: "reviewTask",
    method: "POST",
    path: "/tasks/7/review",
    query: [],
    body: [],
    dial: (c) => c.reviewTask(7) satisfies Promise<TaskExport>,
  },
  {
    name: "pauseTask",
    method: "POST",
    path: "/tasks/7/pause",
    query: [],
    body: [],
    dial: (c) => c.pauseTask(7) satisfies Promise<TaskExport>,
  },
  {
    name: "resumeTask",
    method: "POST",
    path: "/tasks/7/resume",
    query: [],
    body: ["force"],
    dial: (c) => c.resumeTask(7, { force: true }) satisfies Promise<TaskExport>,
  },
  {
    name: "noteTask",
    method: "POST",
    path: "/tasks/7/note",
    query: [],
    body: ["note"],
    dial: (c) => c.noteTask(7, { note: "n" }) satisfies Promise<TaskExport>,
  },
  {
    name: "closeTask",
    method: "POST",
    path: "/tasks/7/close",
    query: [],
    body: ["force", "reason", "summary"],
    dial: (c) =>
      c.closeTask(7, {
        reason: "completed",
        summary: "s",
        force: true,
      }) satisfies Promise<TaskExport>,
  },
  {
    name: "reopenTask",
    method: "POST",
    path: "/tasks/7/reopen",
    query: [],
    body: ["force"],
    dial: (c) => c.reopenTask(7, { force: true }) satisfies Promise<TaskExport>,
  },
  {
    name: "archiveTask",
    method: "POST",
    path: "/tasks/7/archive",
    query: [],
    body: ["force"],
    dial: (c) =>
      c.archiveTask(7, { force: true }) satisfies Promise<TaskExport>,
  },
  {
    name: "fetchTaskArchive",
    method: "GET",
    path: "/tasks/7/archive",
    query: [],
    body: [],
    dial: (c) => c.fetchTaskArchive(7) satisfies Promise<Uint8Array>,
  },
  {
    name: "exportTask",
    method: "GET",
    path: "/tasks/7/export",
    query: [],
    body: [],
    dial: (c) => c.exportTask(7) satisfies Promise<TaskExport>,
  },
  {
    name: "exportAllTasks",
    method: "GET",
    path: "/tasks/export",
    query: [],
    body: [],
    dial: (c) => c.exportAllTasks() satisfies Promise<TaskExport[]>,
  },
];

/** The settings surface and the turn-history reads. */
const SETTINGS_HISTORY_ROWS: readonly Row[] = [
  {
    name: "getTimezone",
    method: "GET",
    path: "/settings/timezone",
    query: [],
    body: [],
    dial: (c) =>
      c.getTimezone() satisfies Promise<{ readonly timezone: string | null }>,
  },
  {
    name: "putTimezone",
    method: "PUT",
    path: "/settings/timezone",
    query: [],
    body: ["timezone"],
    dial: (c) =>
      c.putTimezone("UTC") satisfies Promise<{
        readonly timezone: string | null;
      }>,
  },
  {
    name: "history",
    method: "GET",
    path: "/agents/a1/history",
    query: ["before", "day", "limit"],
    body: [],
    dial: (c) =>
      c.history("a1", {
        day: "2026-01-01",
        before: "b",
        limit: 3,
      }) satisfies Promise<TurnsPage>,
  },
  {
    name: "turn",
    method: "GET",
    path: "/agents/a1/history/3",
    query: [],
    body: [],
    dial: (c) => c.turn("a1", 3) satisfies Promise<TurnDetail>,
  },
  {
    name: "callBody",
    method: "GET",
    path: "/agents/a1/history/3/calls/4/body",
    query: ["limit", "offset", "part"],
    body: [],
    dial: (c) =>
      c.callBody("a1", 3, 4, {
        part: "result",
        offset: 0,
        limit: 9,
      }) satisfies Promise<CallBody>,
  },
  {
    name: "messageBody",
    method: "GET",
    path: "/agents/a1/history/3/messages/4/body",
    query: ["limit", "offset", "part"],
    body: [],
    dial: (c) =>
      c.messageBody("a1", 3, 4, {
        part: "text",
        offset: 0,
        limit: 9,
      }) satisfies Promise<MessageBody>,
  },
];

const ALL_ROWS: readonly Row[] = [
  ...AGENT_ROWS,
  ...TASK_ROWS,
  ...SETTINGS_HISTORY_ROWS,
];

/** One dial, one recorded request: a verb that fails after the request
 * went out is irrelevant to this test, since the request is what is read.
 * The `calls.length` assertion below is what keeps that harmless — a dial
 * that never fires records nothing and fails there. */
async function dialOnce(
  client: TagmaClient,
  row: Row,
): Promise<Array<{ url: string; init?: RequestInit }>> {
  const calls: Array<{ url: string; init?: RequestInit }> = [];
  await withFetch(
    (url, init) => {
      calls.push({ url: String(url), init });
      return answer(String(url));
    },
    async () => {
      await row.dial(client).catch(() => undefined);
    },
  );
  return calls;
}

/** Prototype members that are plumbing rather than verbs. Each is asserted to
 * exist, so renaming one fails here instead of silently moving a method into
 * the verb set. */
const NOT_VERBS: readonly string[] = [
  "authHeaders",
  "bytes",
  "constructor",
  "json",
  "raw",
  "status",
];

Deno.test(
  "every verb dials the method, path, query keys and body keys its row names",
  async () => {
    const client = new TagmaClient({ baseUrl: "http://tagma.test" });
    for (const row of ALL_ROWS) {
      const calls = await dialOnce(client, row);
      assertEquals(
        calls.length,
        1,
        `${row.name}: dialled ${calls.length} requests`,
      );
      const [call] = calls;
      if (call === undefined) continue;
      const url = new URL(call.url);
      assertEquals(
        call.init?.method ?? "GET",
        row.method,
        `${row.name}: method`,
      );
      assertEquals(url.pathname, row.path, `${row.name}: path`);
      assertEquals(
        [...url.searchParams.keys()].sort(),
        [...row.query],
        `${row.name}: query keys`,
      );
      assertEquals(
        sentBodyKeys(call.init),
        [...row.body],
        `${row.name}: body keys`,
      );
    }
  },
);

Deno.test("the table names every prototype method and nothing else", () => {
  const proto = Object.getOwnPropertyNames(TagmaClient.prototype);
  for (const helper of NOT_VERBS) {
    assertEquals(proto.includes(helper), true, `prototype lost ${helper}`);
  }
  const verbs = proto.filter((name) => !NOT_VERBS.includes(name)).sort();
  assertEquals(verbs, ALL_ROWS.map((row) => row.name).sort());
});

/** Every non-test TypeScript file under this package's src/, subdirectories
 * included, so a bypass moved into a nested module stays caught. Only `.ts`
 * is read (`.tsx` and `.mts` are not), and test files are skipped by suffix.
 * Reading needs `--allow-read=src`, resolved against the cwd: the package's
 * test script runs from the package, so a hand-run from the repo root would
 * fail on permissions rather than on a finding. */
async function* sourceFiles(dir: URL): AsyncGenerator<URL> {
  const pending: URL[] = [dir];
  while (pending.length > 0) {
    const current = pending.pop();
    if (current === undefined) continue;
    for await (const entry of Deno.readDir(current)) {
      const child = new URL(
        entry.name + (entry.isDirectory ? "/" : ""),
        current,
      );
      if (entry.isDirectory) {
        pending.push(child);
      } else if (
        entry.name.endsWith(".ts") &&
        !entry.name.endsWith("_test.ts") &&
        !entry.name.endsWith(".test.ts")
      ) {
        yield child;
      }
    }
  }
}

Deno.test("no source file outside this test calls fetch directly", async () => {
  // A tripwire, not a proof: `fetch (` with a space, a reference taken once
  // and called later, and `Reflect.apply` all slip past a substring count,
  // while a comment mentioning `fetch(` turns it red. What it catches is the
  // plain call, which is the shape a bypass takes in practice.
  let hits = 0;
  let scanned = 0;
  for await (const file of sourceFiles(new URL("./", import.meta.url))) {
    scanned += 1;
    const text = await Deno.readTextFile(file);
    hits += text.split("fetch(").length - 1;
  }
  assertEquals(scanned > 0, true, "the scan found no source file");
  assertEquals(hits, 0, "an HTTP call bypasses the shared seam");
});
