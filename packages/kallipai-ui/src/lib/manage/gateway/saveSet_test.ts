import { assertEquals } from "@std/assert";
import type { AdminProfileRow, AdminSetRow } from "./client.ts";
import {
  saveSetDraft,
  saveStoppedBanner,
  setPatchBody,
  type SetPatchBody,
  type SetSavePorts,
  type StepResult,
} from "./saveSet.ts";
import {
  changedUpdates,
  memberEditOf,
  type MemberUpdate,
  type MemberUpdateBody,
  type SetSaveDraft,
} from "./setDraft.ts";
import {
  manage_gateway_set_save_members,
  manage_gateway_set_save_partial,
} from "../../../paraglide/messages.js";

// The set dialog's save sequence, driven through fake ports: which
// requests run, in what order, how often the set is read back, and what
// the dialog is told it may keep.

function row(
  id: string,
  overrides: Partial<AdminProfileRow> = {},
): AdminProfileRow {
  return {
    profile_id: id,
    collection_name: "acme",
    provider_id: id,
    owner: "system",
    family: "deepseek",
    model: `${id}-model`,
    max_context_window: 128_000,
    effort: null,
    modalities: ["text"],
    parked: false,
    max_budget: null,
    tpm_limit: null,
    rpm_limit: null,
    ...overrides,
  };
}

function set(
  profiles: AdminProfileRow[],
  description = "the primary rotation",
): AdminSetRow {
  return { collection_name: "acme", name: "primary", description, profiles };
}

// A row whose model changed, through the same arithmetic the dialogs run,
// so the fixture cannot drift from the body a save really sends.
function change(id: string): MemberUpdate {
  const original = row(id);
  const updates = changedUpdates([original], [original], {
    [id]: { ...memberEditOf(original), model: `${id}-next` },
  });
  return updates[0]!;
}

/** What the fake ports refuse, and whether the refresh throws. */
interface RecorderOptions {
  refusePut?: (index: number) => boolean;
  refusePatch?: boolean;
  throwOnSettle?: boolean;
}

interface Recorder {
  ports: SetSavePorts<AdminSetRow>;
  /** Every port the sequence reached, in order, the refresh and the
   * re-read included. */
  log: string[];
  rereads: () => number;
  settles: () => number;
}

function recorder(options: RecorderOptions = {}): Recorder {
  const log: string[] = [];
  let rereads = 0;
  let settles = 0;
  let puts = 0;
  const ports: SetSavePorts<AdminSetRow> = {
    putMember: (profileId) => {
      const index = puts++;
      log.push(`put ${profileId}`);
      const ok = !(options.refusePut?.(index) ?? false);
      const verdict: StepResult = {
        ok,
        reason: ok ? null : `${profileId} was refused`,
      };
      return Promise.resolve(verdict);
    },
    patchSet: (body) => {
      log.push(`patch ${JSON.stringify(body)}`);
      const ok = !(options.refusePatch ?? false);
      const verdict: StepResult = {
        ok,
        reason: ok ? null : "the set was refused",
      };
      return Promise.resolve(verdict);
    },
    settle: () => {
      settles++;
      log.push("settle");
      if (options.throwOnSettle ?? false) {
        throw new Error("the read side is stale");
      }
    },
    reread: () => {
      rereads++;
      log.push("reread");
      return Promise.resolve(undefined);
    },
  };
  return { ports, log, rereads: () => rereads, settles: () => settles };
}

Deno.test("member rows land before the order they are named in", async () => {
  const original = set([row("p1"), row("p2")]);
  const draft: SetSaveDraft = {
    description: original.description,
    members: ["p2", "p1"],
    updates: [change("p2"), change("p1")],
  };
  const rec = recorder();
  const outcome = await saveSetDraft(original, draft, rec.ports);
  assertEquals(outcome.phase, "done");
  assertEquals(rec.log, [
    "put p2",
    "put p1",
    'patch {"members":["p2","p1"]}',
    "settle",
  ]);
  assertEquals(rec.settles(), 1);
  assertEquals(rec.rereads(), 0);
});

Deno.test(
  "a refused member PUT stops before the PATCH and re-reads once",
  async () => {
    const original = set([row("p1"), row("p2"), row("p3")]);
    const draft: SetSaveDraft = {
      description: original.description,
      members: ["p1", "p2", "p3"],
      updates: [change("p1"), change("p2"), change("p3")],
    };
    const rec = recorder({ refusePut: (index) => index === 1 });
    const outcome = await saveSetDraft(original, draft, rec.ports);
    assertEquals(outcome.phase, "put");
    assertEquals(outcome.landed, ["p1"]);
    assertEquals(outcome.total, 3);
    assertEquals(outcome.reason, "p2 was refused");
    assertEquals(outcome.patchPending, false);
    // The third row and the order never leave the dialog.
    assertEquals(rec.log, ["put p1", "put p2", "settle", "reread"]);
    assertEquals(rec.settles(), 1);
    assertEquals(rec.rereads(), 1);
  },
);

Deno.test(
  "a refused PATCH keeps every landed row and reads the set back",
  async () => {
    const original = set([row("p1"), row("p2")], "old");
    const draft: SetSaveDraft = {
      description: "new",
      members: ["p2", "p1"],
      updates: [change("p1"), change("p2")],
    };
    const rec = recorder({ refusePatch: true });
    const outcome = await saveSetDraft(original, draft, rec.ports);
    assertEquals(outcome.phase, "patch");
    assertEquals(outcome.landed, ["p1", "p2"]);
    assertEquals(outcome.patchPending, true);
    assertEquals(outcome.reason, "the set was refused");
    assertEquals(rec.log, [
      "put p1",
      "put p2",
      'patch {"description":"new","members":["p2","p1"]}',
      "settle",
      "reread",
    ]);
    assertEquals(rec.settles(), 1);
    assertEquals(rec.rereads(), 1);
  },
);

Deno.test("a draft that matches the row sends nothing", async () => {
  const original = set([row("p1")]);
  const draft: SetSaveDraft = {
    description: original.description,
    members: ["p1"],
    updates: [],
  };
  const rec = recorder();
  const outcome = await saveSetDraft(original, draft, rec.ports);
  assertEquals(outcome.phase, "done");
  assertEquals(outcome.patchPending, false);
  assertEquals(rec.log, ["settle"]);
  assertEquals(rec.settles(), 1);
  assertEquals(rec.rereads(), 0);
});

Deno.test("a content-only save never sends the order", async () => {
  const original = set([row("p1"), row("p2")]);
  const draft: SetSaveDraft = {
    description: original.description,
    members: ["p1", "p2"],
    updates: [change("p1"), change("p2")],
  };
  const rec = recorder();
  const outcome = await saveSetDraft(original, draft, rec.ports);
  assertEquals(outcome.phase, "done");
  assertEquals(outcome.patchPending, false);
  assertEquals(rec.log, ["put p1", "put p2", "settle"]);
  assertEquals(rec.settles(), 1);
});

Deno.test("the PATCH carries only the part of the draft that changed", () => {
  const original = set([row("p1"), row("p2")], "old");
  const unchanged: SetSaveDraft = {
    description: "old",
    members: ["p1", "p2"],
    updates: [],
  };
  // A stale draft cannot rewrite an untouched order: the members stay
  // off the wire unless the dialog changes them.
  assertEquals(setPatchBody(original, unchanged), null);
  const described: SetPatchBody | null = setPatchBody(original, {
    ...unchanged,
    description: "new",
  });
  assertEquals(described, { description: "new" });
  assertEquals(
    setPatchBody(original, { ...unchanged, members: ["p2", "p1"] }),
    { members: ["p2", "p1"] },
  );
  // A removal is a membership change like any other.
  assertEquals(setPatchBody(original, { ...unchanged, members: ["p1"] }), {
    members: ["p1"],
  });
});

Deno.test("emptying the member list sends members: []", async () => {
  const original = set([row("p1"), row("p2")]);
  const draft: SetSaveDraft = {
    description: original.description,
    members: [],
    updates: [],
  };
  const rec = recorder();
  const outcome = await saveSetDraft(original, draft, rec.ports);
  assertEquals(outcome.phase, "done");
  assertEquals(outcome.patchPending, true);
  // The last member leaving is a membership change: the description stays
  // off the wire, and the empty order travels on its own.
  assertEquals(rec.log, ['patch {"members":[]}', "settle"]);
  assertEquals(rec.settles(), 1);
});
Deno.test(
  "a window field reaches the PUT as the count the wire takes",
  async () => {
    const original = set([row("p1")]);
    const edit = { ...memberEditOf(row("p1")), context_window: " 200000 " };
    const draft: SetSaveDraft = {
      description: original.description,
      members: ["p1"],
      updates: changedUpdates(original.profiles, original.profiles, {
        p1: edit,
      }),
    };
    const bodies: MemberUpdateBody[] = [];
    const ports: SetSavePorts<AdminSetRow> = {
      putMember: (_id, body) => {
        bodies.push(body);
        return Promise.resolve({ ok: true, reason: null });
      },
      patchSet: () => Promise.resolve({ ok: true, reason: null }),
      settle: () => {},
      reread: () => Promise.resolve(undefined),
    };
    const outcome = await saveSetDraft(original, draft, ports);
    assertEquals(outcome.phase, "done");
    assertEquals(outcome.landed, ["p1"]);
    assertEquals(bodies, [
      {
        model: "p1-model",
        max_context_window: 200_000,
        effort: null,
        modalities: null,
      },
    ]);
  },
);

Deno.test("a stop with nothing landed reports only the server's reason", () => {
  assertEquals(
    saveStoppedBanner({
      landed: [],
      total: 3,
      patchPending: false,
      reason: "p1 was refused",
    }),
    "p1 was refused",
  );
  // Nothing landed and no reason: nothing to say.
  assertEquals(
    saveStoppedBanner({
      landed: [],
      total: 0,
      patchPending: false,
      reason: null,
    }),
    null,
  );
});

Deno.test("a stop with rows landed counts them and names the step", () => {
  const counts = { saved: 1, total: 3 };
  assertEquals(
    saveStoppedBanner({
      landed: ["p1"],
      total: 3,
      patchPending: true,
      reason: null,
    }),
    manage_gateway_set_save_partial(counts),
  );
  assertEquals(
    saveStoppedBanner({
      landed: ["p1"],
      total: 3,
      patchPending: false,
      reason: null,
    }),
    manage_gateway_set_save_members(counts),
  );
  // The count comes first and the stopping reason follows it.
  assertEquals(
    saveStoppedBanner({
      landed: ["p1"],
      total: 3,
      patchPending: true,
      reason: "p2 was refused",
    }),
    `${manage_gateway_set_save_partial(counts)} p2 was refused`,
  );
});

// Every way the sequence can end, each with the port order it leaves
// behind: the refresh sits after the last write and before the re-read
// when there is one, and it runs on every exit exactly once.
Deno.test("each exit settles once, after its last write", async (t) => {
  const cases: Array<{
    name: string;
    original: AdminSetRow;
    draft: SetSaveDraft;
    options: RecorderOptions;
    phase: "done" | "put" | "patch";
    log: string[];
  }> = [
    {
      name: "every write lands, the PATCH last",
      original: set([row("p1")], "old"),
      draft: { description: "new", members: ["p1"], updates: [change("p1")] },
      options: {},
      phase: "done",
      log: ["put p1", 'patch {"description":"new"}', "settle"],
    },
    {
      name: "nothing needs a PATCH",
      original: set([row("p1")]),
      draft: {
        description: "the primary rotation",
        members: ["p1"],
        updates: [change("p1")],
      },
      options: {},
      phase: "done",
      log: ["put p1", "settle"],
    },
    {
      name: "a member PUT is refused",
      original: set([row("p1"), row("p2")]),
      draft: {
        description: "the primary rotation",
        members: ["p1", "p2"],
        updates: [change("p1"), change("p2")],
      },
      options: { refusePut: () => true },
      phase: "put",
      log: ["put p1", "settle", "reread"],
    },
    {
      name: "the PATCH is refused",
      original: set([row("p1")], "old"),
      draft: { description: "new", members: ["p1"], updates: [] },
      options: { refusePatch: true },
      phase: "patch",
      log: ['patch {"description":"new"}', "settle", "reread"],
    },
  ];
  for (const item of cases) {
    await t.step(item.name, async () => {
      const rec = recorder(item.options);
      const outcome = await saveSetDraft(item.original, item.draft, rec.ports);
      assertEquals(outcome.phase, item.phase);
      assertEquals(rec.log, item.log);
      assertEquals(rec.settles(), 1);
    });
  }
});

Deno.test("a refresh that throws does not take the outcome away", async () => {
  const original = set([row("p1")], "old");
  const draft: SetSaveDraft = {
    description: "new",
    members: ["p1"],
    updates: [],
  };
  const rec = recorder({ refusePatch: true, throwOnSettle: true });
  const outcome = await saveSetDraft(original, draft, rec.ports);
  assertEquals(outcome.phase, "patch");
  assertEquals(outcome.reason, "the set was refused");
  assertEquals(rec.settles(), 1);
  assertEquals(rec.rereads(), 1);
});
