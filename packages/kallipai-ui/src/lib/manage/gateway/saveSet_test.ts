import { assertEquals } from "@std/assert";
import type { AdminProfileRow, AdminSetRow } from "./client.ts";
import {
  saveSetDraft,
  saveStoppedBanner,
  type SetSaveOutcome,
  type SetSavePhase,
  type SetSavePorts,
  type StepResult,
} from "./saveSet.ts";
import {
  memberEditOf,
  memberIds,
  membershipWrites,
  orderToSend,
  type MemberEdit,
  type MemberWriteBody,
  type SetSaveDraft,
} from "./setDraft.ts";
import {
  manage_gateway_set_save_description,
  manage_gateway_set_save_description_only,
  manage_gateway_set_save_members,
  manage_gateway_set_save_order,
  manage_gateway_set_save_order_only,
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
    set: "primary",
    position: 0,
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

/** A member body as the wire spells it: a log line names the exact
 * request, not a shape the test half-remembers. */
function wire(
  id: string,
  membership: string | null,
  overrides: Partial<MemberWriteBody> = {},
): string {
  return JSON.stringify({
    model: `${id}-model`,
    max_context_window: 128_000,
    effort: null,
    modalities: ["text"],
    set: membership,
    ...overrides,
  });
}

/** The body an edited row sends: the form's fields, where a text-only tag
 * selection rides as absent (the server's default). */
function wireEdited(
  id: string,
  membership: string | null,
  overrides: Partial<MemberWriteBody> = {},
): string {
  return wire(id, membership, { modalities: null, ...overrides });
}

/** A row's edit with its model renamed, through the same arithmetic the
 * dialogs run, so a fixture cannot drift from a body a save sends. */
function rename(id: string, model: string): MemberEdit {
  return { ...memberEditOf(row(id)), model };
}

/** The draft a dialog hands back for a set: the shown order, plus the
 * writes the shared arithmetic derives from it. */
function draftOf(
  original: AdminSetRow,
  members: AdminProfileRow[],
  edits: Record<string, MemberEdit> = {},
  description = original.description,
  name = original.name,
): SetSaveDraft {
  return {
    name,
    description,
    members: memberIds(members),
    ...membershipWrites(original.profiles, members, edits, name),
  };
}

/** What the fake ports refuse, and whether the refresh throws. */
interface RecorderOptions {
  refusePut?: (index: number) => boolean;
  refuseDescription?: boolean;
  refuseOrder?: boolean;
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
    putMember: (profileId, body) => {
      const index = puts++;
      log.push(`put ${profileId} ${JSON.stringify(body)}`);
      const ok = !(options.refusePut?.(index) ?? false);
      const verdict: StepResult = {
        ok,
        reason: ok ? null : `${profileId} was refused`,
      };
      return Promise.resolve(verdict);
    },
    patchDescription: (description) => {
      log.push(`description ${description}`);
      const ok = !(options.refuseDescription ?? false);
      const verdict: StepResult = {
        ok,
        reason: ok ? null : "the description was refused",
      };
      return Promise.resolve(verdict);
    },
    reorder: (members) => {
      log.push(`order ${members.join(",")}`);
      const ok = !(options.refuseOrder ?? false);
      const verdict: StepResult = {
        ok,
        reason: ok ? null : "the order was refused",
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

/** An outcome as a test reads it: the failing fields, the two writes the
 * draft carried, and a re-read the fake ports never answer. */
function outcomeOf(
  phase: SetSavePhase,
  landed: string[],
  total: number,
  descriptionWrite: boolean,
  orderWrite: boolean,
  reason: string | null = null,
): SetSaveOutcome<AdminSetRow> {
  return {
    phase,
    landed,
    total,
    descriptionWrite,
    orderWrite,
    reason,
    row: undefined,
  };
}

Deno.test(
  "kept edits, joins and leaves land before the description and the order",
  async () => {
    const original = set([row("p1"), row("p2"), row("p3")], "old");
    const joined = row("p9", { set: null });
    const draft = draftOf(
      original,
      [row("p2"), joined, row("p3")],
      { p2: rename("p2", "p2-next") },
      "new",
    );
    const fake = recorder();
    const outcome = await saveSetDraft(original, draft, fake.ports);
    assertEquals(fake.log, [
      `put p2 ${wireEdited("p2", "primary", { model: "p2-next" })}`,
      `put p9 ${wire("p9", "primary")}`,
      `put p1 ${wire("p1", null)}`,
      "description new",
      "order p2,p9,p3",
      "settle",
    ]);
    assertEquals(outcome, outcomeOf("done", ["p2", "p9", "p1"], 3, true, true));
    assertEquals(fake.rereads(), 0);
  },
);

Deno.test("a refused member PUT stops before the description", async () => {
  const original = set([row("p1"), row("p2"), row("p3")], "old");
  const draft = draftOf(
    original,
    [row("p1"), row("p2"), row("p3")],
    {
      p1: rename("p1", "p1-next"),
      p2: rename("p2", "p2-next"),
      p3: rename("p3", "p3-next"),
    },
    "new",
  );
  const fake = recorder({ refusePut: (index) => index === 1 });
  const outcome = await saveSetDraft(original, draft, fake.ports);
  assertEquals(fake.log, [
    `put p1 ${wireEdited("p1", "primary", { model: "p1-next" })}`,
    `put p2 ${wireEdited("p2", "primary", { model: "p2-next" })}`,
    "settle",
    "reread",
  ]);
  assertEquals(
    outcome,
    outcomeOf("put", ["p1"], 3, true, false, "p2 was refused"),
  );
  assertEquals(fake.rereads(), 1);
});

Deno.test(
  "a refused description stop names no order it never had",
  async () => {
    const original = set([row("p1"), row("p2")]);
    const draft = draftOf(original, [row("p1"), row("p2")], {}, "new");
    const fake = recorder({ refuseDescription: true });
    const outcome = await saveSetDraft(original, draft, fake.ports);
    assertEquals(fake.log, ["description new", "settle", "reread"]);
    assertEquals(
      outcome,
      outcomeOf(
        "description",
        [],
        0,
        true,
        false,
        "the description was refused",
      ),
    );
    // Nothing landed, so the banner gives the reason alone: it cannot report a
    // membership save, and there was no order write to mention.
    assertEquals(saveStoppedBanner(outcome), "the description was refused");
  },
);

Deno.test("a refused order stops after the membership", async () => {
  const original = set([row("p1"), row("p2")]);
  const draft = draftOf(original, [row("p2"), row("p1")], {
    p1: rename("p1", "p1-next"),
  });
  const fake = recorder({ refuseOrder: true });
  const outcome = await saveSetDraft(original, draft, fake.ports);
  assertEquals(fake.log, [
    `put p1 ${wireEdited("p1", "primary", { model: "p1-next" })}`,
    "order p2,p1",
    "settle",
    "reread",
  ]);
  assertEquals(
    outcome,
    outcomeOf("order", ["p1"], 1, false, true, "the order was refused"),
  );
  // The description never changed, so the banner names the order alone.
  assertEquals(
    saveStoppedBanner(outcome),
    `${manage_gateway_set_save_order_only()} the order was refused`,
  );
});

Deno.test("a reorder-only stop reports no membership save", async () => {
  const original = set([row("p1"), row("p2")]);
  const draft = draftOf(original, [row("p2"), row("p1")]);
  const fake = recorder({ refuseOrder: true });
  const outcome = await saveSetDraft(original, draft, fake.ports);
  assertEquals(fake.log, ["order p2,p1", "settle", "reread"]);
  assertEquals(
    outcome,
    outcomeOf("order", [], 0, false, true, "the order was refused"),
  );
  // No membership write ran and no description write existed: the reason is
  // the whole report.
  assertEquals(saveStoppedBanner(outcome), "the order was refused");
});

Deno.test("a description stop names the order it still owed", async () => {
  const original = set([row("p1"), row("p2")], "old");
  const draft = draftOf(
    original,
    [row("p2"), row("p1")],
    { p1: rename("p1", "p1-next") },
    "new",
  );
  const fake = recorder({ refuseDescription: true });
  const outcome = await saveSetDraft(original, draft, fake.ports);
  assertEquals(fake.log, [
    `put p1 ${wireEdited("p1", "primary", { model: "p1-next" })}`,
    "description new",
    "settle",
    "reread",
  ]);
  assertEquals(
    outcome,
    outcomeOf(
      "description",
      ["p1"],
      1,
      true,
      true,
      "the description was refused",
    ),
  );
  assertEquals(
    saveStoppedBanner(outcome),
    `${manage_gateway_set_save_description()} the description was refused`,
  );
});

Deno.test("a description stop with no order owed names it alone", async () => {
  const original = set([row("p1"), row("p2")], "old");
  const draft = draftOf(
    original,
    [row("p1"), row("p2")],
    { p1: rename("p1", "p1-next") },
    "new",
  );
  const fake = recorder({ refuseDescription: true });
  const outcome = await saveSetDraft(original, draft, fake.ports);
  assertEquals(
    outcome,
    outcomeOf(
      "description",
      ["p1"],
      1,
      true,
      false,
      "the description was refused",
    ),
  );
  assertEquals(
    saveStoppedBanner(outcome),
    `${manage_gateway_set_save_description_only()} the description was refused`,
  );
});

Deno.test("a draft that matches the row sends nothing", async () => {
  const original = set([row("p1"), row("p2")]);
  const draft = draftOf(original, [row("p1"), row("p2")]);
  const fake = recorder();
  const outcome = await saveSetDraft(original, draft, fake.ports);
  assertEquals(fake.log, ["settle"]);
  assertEquals(outcome, outcomeOf("done", [], 0, false, false));
});

Deno.test("a content-only save never sends the order", async () => {
  const original = set([row("p1"), row("p2")]);
  const draft = draftOf(original, [row("p1"), row("p2")], {
    p2: rename("p2", "p2-next"),
  });
  const fake = recorder();
  const outcome = await saveSetDraft(original, draft, fake.ports);
  assertEquals(fake.log, [
    `put p2 ${wireEdited("p2", "primary", { model: "p2-next" })}`,
    "settle",
  ]);
  assertEquals(outcome, outcomeOf("done", ["p2"], 1, false, false));
});

Deno.test("every member leaving sends the leaves and no order", async () => {
  const original = set([row("p1"), row("p2")]);
  const draft = draftOf(original, []);
  const fake = recorder();
  const outcome = await saveSetDraft(original, draft, fake.ports);
  assertEquals(fake.log, [
    `put p1 ${wire("p1", null)}`,
    `put p2 ${wire("p2", null)}`,
    "settle",
  ]);
  assertEquals(outcome, outcomeOf("done", ["p1", "p2"], 2, false, false));
});

Deno.test("a row that leaves carries the draft's edit", async () => {
  const original = set([row("p1"), row("p2")]);
  const draft = draftOf(original, [row("p2")], { p1: rename("p1", "p1-next") });
  const fake = recorder();
  const outcome = await saveSetDraft(original, draft, fake.ports);
  assertEquals(fake.log, [
    `put p1 ${wireEdited("p1", null, { model: "p1-next" })}`,
    "settle",
  ]);
  assertEquals(outcome, outcomeOf("done", ["p1"], 1, false, false));
});

Deno.test(
  "the order goes out only when the writes leave another sequence",
  () => {
    const cases: Array<[string[], string[], string[] | null]> = [
      [["p1", "p2"], [], null],
      [
        ["p1", "p2", "p3"],
        ["p3", "p1", "p2"],
        ["p3", "p1", "p2"],
      ],
      [["p1", "p2"], ["p1", "p2", "p9"], null],
      [
        ["p1", "p2"],
        ["p9", "p1", "p2"],
        ["p9", "p1", "p2"],
      ],
    ];
    for (const [originalIds, draftIds, expected] of cases) {
      assertEquals(
        orderToSend(originalIds, draftIds),
        expected,
        draftIds.join(","),
      );
    }
  },
);

Deno.test(
  "the context field reaches the PUT as the number it spells",
  async () => {
    const original = set([row("p1")]);
    const edit = { ...memberEditOf(row("p1")), context_window: " 200000 " };
    const draft = draftOf(original, [row("p1")], { p1: edit });
    const fake = recorder();
    const outcome = await saveSetDraft(original, draft, fake.ports);
    assertEquals(fake.log, [
      `put p1 ${wireEdited("p1", "primary", { max_context_window: 200_000 })}`,
      "settle",
    ]);
    assertEquals(outcome, outcomeOf("done", ["p1"], 1, false, false));
  },
);

Deno.test(
  "the banner names the step the save stopped at and what it left unsent",
  () => {
    const counts = { saved: 1, total: 3 };
    // A save whose first write was refused has nothing landed to report.
    assertEquals(
      saveStoppedBanner({
        phase: "put",
        landed: [],
        total: 3,
        descriptionWrite: true,
        orderWrite: true,
        reason: "p1 was refused",
      }),
      "p1 was refused",
    );
    assertEquals(
      saveStoppedBanner({
        phase: "put",
        landed: [],
        total: 3,
        descriptionWrite: true,
        orderWrite: true,
        reason: null,
      }),
      null,
    );
    assertEquals(
      saveStoppedBanner({
        phase: "put",
        landed: ["p1"],
        total: 3,
        descriptionWrite: true,
        orderWrite: true,
        reason: null,
      }),
      manage_gateway_set_save_partial(counts),
    );
    assertEquals(
      saveStoppedBanner({
        phase: "put",
        landed: ["p1"],
        total: 3,
        descriptionWrite: false,
        orderWrite: false,
        reason: null,
      }),
      manage_gateway_set_save_members(counts),
    );
    assertEquals(
      saveStoppedBanner({
        phase: "put",
        landed: ["p1"],
        total: 3,
        descriptionWrite: false,
        orderWrite: true,
        reason: "the order was refused",
      }),
      `${manage_gateway_set_save_partial(counts)} the order was refused`,
    );
    // A description stop names the order only when the draft carried one.
    assertEquals(
      saveStoppedBanner({
        phase: "description",
        landed: ["p1"],
        total: 3,
        descriptionWrite: true,
        orderWrite: true,
        reason: "the order was refused",
      }),
      `${manage_gateway_set_save_description()} the order was refused`,
    );
    assertEquals(
      saveStoppedBanner({
        phase: "description",
        landed: ["p1"],
        total: 3,
        descriptionWrite: true,
        orderWrite: false,
        reason: null,
      }),
      manage_gateway_set_save_description_only(),
    );
    // An order stop names the description only when the draft carried one.
    assertEquals(
      saveStoppedBanner({
        phase: "order",
        landed: ["p1"],
        total: 3,
        descriptionWrite: true,
        orderWrite: true,
        reason: "the order was refused",
      }),
      `${manage_gateway_set_save_order()} the order was refused`,
    );
    assertEquals(
      saveStoppedBanner({
        phase: "order",
        landed: ["p1"],
        total: 3,
        descriptionWrite: false,
        orderWrite: true,
        reason: null,
      }),
      manage_gateway_set_save_order_only(),
    );
    // A stop that landed nothing, a reorder-only save among them, gives the
    // reason alone: it cannot report a membership save that did not run.
    assertEquals(
      saveStoppedBanner({
        phase: "order",
        landed: [],
        total: 0,
        descriptionWrite: false,
        orderWrite: true,
        reason: "the order was refused",
      }),
      "the order was refused",
    );
    assertEquals(
      saveStoppedBanner({
        phase: "description",
        landed: [],
        total: 0,
        descriptionWrite: true,
        orderWrite: false,
        reason: "the description was refused",
      }),
      "the description was refused",
    );
  },
);

Deno.test("each exit settles once, after its last write", async () => {
  const plain = set([row("p1")], "old");
  const edited = set([row("p1"), row("p2")]);
  const cases: Array<{
    options: RecorderOptions;
    saved: AdminSetRow;
    draft: SetSaveDraft;
    phase: SetSavePhase;
  }> = [
    {
      options: {},
      saved: plain,
      draft: draftOf(plain, [row("p1")]),
      phase: "done",
    },
    {
      options: {},
      saved: edited,
      draft: draftOf(edited, [row("p1"), row("p2")], {
        p2: rename("p2", "p2-next"),
      }),
      phase: "done",
    },
    {
      options: { refusePut: () => true },
      saved: edited,
      draft: draftOf(edited, [row("p1"), row("p2")], {
        p1: rename("p1", "p1-next"),
      }),
      phase: "put",
    },
    {
      options: { refuseDescription: true },
      saved: edited,
      draft: draftOf(edited, [row("p1"), row("p2")], {}, "new"),
      phase: "description",
    },
    {
      options: { refuseOrder: true },
      saved: edited,
      draft: draftOf(edited, [row("p2"), row("p1")]),
      phase: "order",
    },
  ];
  for (const { options, saved, draft, phase } of cases) {
    const fake = recorder(options);
    const outcome = await saveSetDraft(saved, draft, fake.ports);
    assertEquals(outcome.phase, phase);
    assertEquals(fake.settles(), 1, `${phase} settles once`);
    const at = fake.log.indexOf("settle");
    // Only the failure's re-read may follow the refresh.
    assertEquals(
      fake.log.slice(at + 1).every((line) => line === "reread"),
      true,
    );
    // The refresh follows the last write, so nothing ran after it.
    assertEquals(
      fake.log.indexOf("reread"),
      outcome.phase === "done" ? -1 : at + 1,
    );
  }
});

Deno.test("a refresh that throws does not take the outcome away", async () => {
  const original = set([row("p1")], "old");
  const draft = draftOf(original, [row("p1")], {}, "new");
  const fake = recorder({ refuseDescription: true, throwOnSettle: true });
  const outcome = await saveSetDraft(original, draft, fake.ports);
  assertEquals(
    outcome,
    outcomeOf("description", [], 0, true, false, "the description was refused"),
  );
  assertEquals(fake.settles(), 1);
  assertEquals(fake.rereads(), 1);
});
