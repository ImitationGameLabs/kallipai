import { assertEquals, assertExists } from "@std/assert";

import type { UserProfileRow, UserSetRow } from "./client.ts";
import { createSetDraft, type CreateSetPorts } from "./createSet.ts";
import type { SetSaveDraft } from "./setDraft.ts";

// The create's pre-flight, driven through fake ports: whether the row read
// runs at all, whether a refusal stops the write, and what the draft the
// sequence walks carries afterwards.

function profileRow(id: string): UserProfileRow {
  return {
    profile_id: id,
    collection_name: "acme",
    provider_id: id,
    family: "deepseek",
    model: `${id}-model`,
    max_context_window: 128_000,
    effort: null,
    modalities: ["text"],
    max_budget: null,
    tpm_limit: null,
    rpm_limit: null,
    set: null,
    position: 0,
  };
}

/** The draft a dialog hands back for a set that does not exist yet: the
 * picks' ids and nothing else, since nothing is there to diff against. */
function draftOf(members: string[], name = "primary"): SetSaveDraft {
  return {
    name,
    description: "the primary rotation",
    members,
    updates: [],
    joins: [],
    leaves: [],
  };
}

/** What the fake read side answers, and when. `refresh` runs between the two
 * reads a create makes, so a test can open the rows mid-way. */
interface Fake {
  rows: () => readonly UserProfileRow[] | null;
  refresh?: () => void;
  create?: (name: string) => UserSetRow | undefined;
}

/** Every port the sequence reached, in order: a test asserts the exact
 * sequence, not a count of calls. */
function recorder(fake: Fake): { ports: CreateSetPorts; log: string[] } {
  const log: string[] = [];
  return {
    log,
    ports: {
      rows: () => {
        log.push("rows");
        return fake.rows();
      },
      refresh: () => {
        log.push("refresh");
        // The store's read side never answers at once, so the fake's own
        // work lands a microtask later too: a fake that flips as it is
        // called would leave the sequence's `await` untested.
        return Promise.resolve().then(() => {
          fake.refresh?.();
        });
      },
      addSet: (name, description) => {
        log.push(`addSet ${name}`);
        const row =
          fake.create === undefined
            ? {
                collection_name: "acme",
                name: `${name}-server`,
                description,
                members: [],
              }
            : fake.create(name);
        return Promise.resolve(row);
      },
    },
  };
}

Deno.test("an empty draft never reads the collection", async () => {
  const { ports, log } = recorder({
    rows: () => {
      throw new Error("an empty create has no picks to resolve");
    },
  });
  const start = await createSetDraft(draftOf([]), ports);
  assertExists(start);
  assertEquals(log, ["addSet primary"]);
  assertEquals(start.target.profiles, []);
  assertEquals(start.draft.members, []);
  assertEquals(start.draft.joins, []);
});

Deno.test(
  "picks the read side cannot answer refuse before the set exists",
  async () => {
    const draft = draftOf(["a", "b"]);
    const { ports, log } = recorder({ rows: () => null });
    const start = await createSetDraft(draft, ports);
    assertEquals(start, undefined);
    // The read runs twice with one refresh between them, and the create
    // never runs: a set built over unread rows would hold no members.
    assertEquals(log, ["rows", "refresh", "rows"]);
    // A refusal leaves the caller's draft exactly as it arrived.
    assertEquals(draft.members, ["a", "b"]);
    assertEquals(draft.joins, []);
    assertEquals(draft.name, "primary");
  },
);

Deno.test(
  "picks resolve against the rows once the read side answers",
  async () => {
    let read = false;
    const { ports, log } = recorder({
      rows: () => (read ? [profileRow("a"), profileRow("b")] : null),
      refresh: () => {
        read = true;
      },
    });
    const start = await createSetDraft(draftOf(["a", "b"]), ports);
    assertEquals(log, ["rows", "refresh", "rows", "addSet primary"]);
    assertEquals(
      start?.draft.joins.map((join) => join.profileId),
      ["a", "b"],
    );
    // The server answers a name of its own: the draft walks under that name,
    // and the member writes carry it as the set they land in.
    assertEquals(start?.draft.name, "primary-server");
    assertEquals(start?.draft.joins[0].body.set, "primary-server");
    assertEquals(start?.draft.members, ["a", "b"]);
    assertEquals(start?.target.name, "primary-server");
  },
);

Deno.test("a failed create answers undefined", async () => {
  const draft = draftOf(["a"]);
  const { ports, log } = recorder({
    rows: () => [profileRow("a")],
    create: () => undefined,
  });
  const start = await createSetDraft(draft, ports);
  assertEquals(start, undefined);
  assertEquals(log, ["rows", "addSet primary"]);
  assertEquals(draft.joins, []);
});
