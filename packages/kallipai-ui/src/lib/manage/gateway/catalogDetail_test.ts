// The detail read's request token: the panel shows the rows of the newest
// read for the collection that is open now, and a switch clears before it
// reads. Every test below goes red if the token compare is dropped.
//
// The module is rune-bearing, but deno test runs it uncompiled, so a
// passthrough $state shim lets it run with plain fields (the store_test
// pattern).

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const { createCatalogDetail } = await import("./catalogDetail.svelte.ts");
import type { AdminProfileRow } from "./client.ts";

/** One row, as the wire spells it; only the id matters to these reads. */
function row(collection: string, id: string): AdminProfileRow {
  return {
    profile_id: id,
    collection_name: collection,
    provider_id: `up-${id}`,
    owner: "admin",
    family: "deepseek",
    model: `${id}-model`,
    max_context_window: 128_000,
    effort: null,
    modalities: ["text"],
    set: null,
    position: 0,
    max_budget: null,
    tpm_limit: null,
    rpm_limit: null,
  };
}

/** A read whose answer the test lands by hand. */
function pending<T>() {
  let land!: (value: T) => void;
  const promise = new Promise<T>((resolve) => {
    land = resolve;
  });
  return { promise, land };
}

Deno.test("a collection switch clears the rows before the read", async () => {
  let open: string | null = "a";
  const reads = new Map<
    string,
    ReturnType<typeof pending<AdminProfileRow[]>>
  >();
  const detail = createCatalogDetail({
    collection: () => open,
    readUnassigned: (name) => {
      const p = pending<AdminProfileRow[]>();
      reads.set(name, p);
      return p.promise;
    },
  });

  const first = detail.refresh();
  reads.get("a")!.land([row("a", "one")]);
  await first;
  assertEquals(
    detail.rows?.map((r) => r.profile_id),
    ["one"],
  );

  open = "b";
  const second = detail.refresh();
  assertEquals(detail.rows, null, "a switch clears first");
  assertEquals(detail.collection, "b");
  reads.get("b")!.land([row("b", "two")]);
  await second;
  assertEquals(
    detail.rows?.map((r) => r.profile_id),
    ["two"],
  );
});

Deno.test(
  "a late answer for a collection that left does not land",
  async () => {
    let open: string | null = "a";
    const reads = new Map<
      string,
      ReturnType<typeof pending<AdminProfileRow[]>>
    >();
    const detail = createCatalogDetail({
      collection: () => open,
      readUnassigned: (name) => {
        const p = pending<AdminProfileRow[]>();
        reads.set(name, p);
        return p.promise;
      },
    });

    const stale = detail.refresh();
    open = "b";
    const fresh = detail.refresh();
    reads.get("b")!.land([row("b", "two")]);
    await fresh;
    reads.get("a")!.land([row("a", "one")]);
    await stale;
    assertEquals(
      detail.rows?.map((r) => r.profile_id),
      ["two"],
    );
  },
);

Deno.test("the newest read of one collection wins", async () => {
  const reads: ReturnType<typeof pending<AdminProfileRow[]>>[] = [];
  const detail = createCatalogDetail({
    collection: () => "a",
    readUnassigned: () => {
      const p = pending<AdminProfileRow[]>();
      reads.push(p);
      return p.promise;
    },
  });

  const older = detail.refresh();
  const newer = detail.refresh();
  reads[1].land([row("a", "new")]);
  await newer;
  reads[0].land([row("a", "old")]);
  await older;
  assertEquals(
    detail.rows?.map((r) => r.profile_id),
    ["new"],
  );
});

Deno.test("a failed read keeps the rows it could not replace", async () => {
  let fail = false;
  const detail = createCatalogDetail({
    collection: () => "a",
    readUnassigned: () =>
      Promise.resolve(fail ? undefined : [row("a", "one"), row("a", "two")]),
  });

  await detail.refresh();
  fail = true;
  await detail.refresh();
  assertEquals(
    detail.rows?.map((r) => r.profile_id),
    ["one", "two"],
  );
});

Deno.test("the list route clears and reads nothing", async () => {
  let open: string | null = "a";
  let reads = 0;
  const detail = createCatalogDetail({
    collection: () => open,
    readUnassigned: () => {
      reads += 1;
      return Promise.resolve([row("a", "one")]);
    },
  });

  await detail.refresh();
  open = null;
  await detail.refresh();
  assertEquals(detail.rows, null);
  assertEquals(detail.collection, null);
  assertEquals(reads, 1, "no read on the list route");
});
