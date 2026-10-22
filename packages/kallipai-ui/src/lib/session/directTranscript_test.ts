// Behaviour tests for the direct-session transcript controller: the hydrate
// walk, the incremental tail pull, and the generation fence that keeps a late
// answer from the pair the reader left out of the pair on screen. The module is
// rune-bearing and `deno test` runs it uncompiled, so it loads behind a
// passthrough $state shim (the seam reveal_test uses).

import { assertEquals } from "@std/assert";
import type { DirectMessageRow } from "@kallipai/kallipai-client";

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { createDirectTranscript } = await import("./directTranscript.svelte.ts");

interface Deferred<T> {
  promise: Promise<T>;
  resolve: (value: T) => void;
  reject: (reason: unknown) => void;
}

function deferred<T>(): Deferred<T> {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

function row(seq: number): DirectMessageRow {
  return { seq } as unknown as DirectMessageRow;
}

function rows(from: number, count: number): DirectMessageRow[] {
  return Array.from({ length: count }, (_, i) => row(from + i));
}

Deno.test("a hydrate answered after the pair changed never lands", async () => {
  const gates: Deferred<DirectMessageRow[]>[] = [];
  const asked: string[] = [];
  const transcript = createDirectTranscript({
    fetchPage: (tagma, peer) => {
      asked.push(`${tagma}:${peer}`);
      const gate = deferred<DirectMessageRow[]>();
      gates.push(gate);
      return gate.promise;
    },
  });

  transcript.open("t1", "p1");
  const first = transcript.refresh();
  transcript.open("t2", "p2");

  gates[0]!.resolve(rows(1, 3));
  await first;

  assertEquals(asked, ["t1:p1"]);
  assertEquals(transcript.rows, []);
  assertEquals(transcript.lastSeq, 0);
  assertEquals(transcript.phase, "loading");
});

Deno.test(
  "an incremental answer for the previous pair is dropped",
  async () => {
    const gates: Deferred<DirectMessageRow[]>[] = [];
    const transcript = createDirectTranscript({
      fetchPage: () => {
        const gate = deferred<DirectMessageRow[]>();
        gates.push(gate);
        return gate.promise;
      },
    });

    transcript.open("t1", "p1");
    const hydrate = transcript.refresh();
    gates[0]!.resolve([row(1), row(2), row(3)]);
    await hydrate;
    assertEquals(transcript.phase, "ready");
    assertEquals(transcript.rows.length, 3);

    const poll = transcript.refresh();
    transcript.open("t2", "p2");
    gates[1]!.resolve([row(4)]);
    await poll;

    assertEquals(transcript.rows, []);
    assertEquals(transcript.lastSeq, 0);
    assertEquals(transcript.phase, "loading");
  },
);

Deno.test(
  "a failure for the previous pair leaves the new one loading",
  async () => {
    const gates: Deferred<DirectMessageRow[]>[] = [];
    const transcript = createDirectTranscript({
      fetchPage: () => {
        const gate = deferred<DirectMessageRow[]>();
        gates.push(gate);
        return gate.promise;
      },
    });

    transcript.open("t1", "p1");
    const first = transcript.refresh();
    transcript.open("t2", "p2");
    gates[0]!.reject(new Error("daemon unreachable"));
    await first;

    assertEquals(transcript.phase, "loading");
  },
);

Deno.test("a failure for the pair on screen marks it unavailable", async () => {
  const gates: Deferred<DirectMessageRow[]>[] = [];
  const transcript = createDirectTranscript({
    fetchPage: () => {
      const gate = deferred<DirectMessageRow[]>();
      gates.push(gate);
      return gate.promise;
    },
  });

  transcript.open("t1", "p1");
  const first = transcript.refresh();
  gates[0]!.reject(new Error("daemon unreachable"));
  await first;
  assertEquals(transcript.phase, "unavailable");

  const retry = transcript.retry();
  assertEquals(transcript.phase, "loading");
  gates[1]!.resolve([row(1), row(2)]);
  await retry;
  assertEquals(transcript.phase, "ready");
  assertEquals(transcript.rows.length, 2);
  assertEquals(transcript.lastSeq, 2);
});

Deno.test("the hydrate walks pages until the daemon runs short", async () => {
  const asked: number[] = [];
  const transcript = createDirectTranscript({
    fetchPage: (_tagma, _peer, afterSeq) => {
      asked.push(afterSeq);
      return Promise.resolve(asked.length === 1 ? rows(1, 25) : rows(26, 2));
    },
  });

  transcript.open("t1", "p1");
  await transcript.refresh();

  assertEquals(asked, [0, 25]);
  assertEquals(transcript.rows.length, 27);
  assertEquals(transcript.lastSeq, 27);
  assertEquals(transcript.phase, "ready");
});

Deno.test("closing drops the answer that was still in flight", async () => {
  const gates: Deferred<DirectMessageRow[]>[] = [];
  const transcript = createDirectTranscript({
    fetchPage: () => {
      const gate = deferred<DirectMessageRow[]>();
      gates.push(gate);
      return gate.promise;
    },
  });

  transcript.open("t1", "p1");
  const first = transcript.refresh();
  transcript.close();
  gates[0]!.resolve(rows(1, 2));
  await first;

  assertEquals(transcript.rows, []);
  assertEquals(transcript.phase, "loading");
});

Deno.test("the increment appends the tail and keeps the last seq", async () => {
  const asked: number[] = [];
  const transcript = createDirectTranscript({
    fetchPage: (_tagma, _peer, afterSeq) => {
      asked.push(afterSeq);
      return Promise.resolve(asked.length === 1 ? rows(1, 2) : rows(3, 1));
    },
  });

  transcript.open("t1", "p1");
  await transcript.refresh();
  await transcript.refresh();

  assertEquals(asked, [0, 2]);
  assertEquals(transcript.rows.length, 3);
  assertEquals(transcript.lastSeq, 3);
});

Deno.test("a later pull supersedes the one already in flight", async () => {
  const gates: Deferred<DirectMessageRow[]>[] = [];
  const asked: number[] = [];
  const transcript = createDirectTranscript({
    fetchPage: (_tagma, _peer, afterSeq) => {
      asked.push(afterSeq);
      const gate = deferred<DirectMessageRow[]>();
      gates.push(gate);
      return gate.promise;
    },
  });

  transcript.open("t1", "p1");
  const hydrate = transcript.refresh();
  gates[0]!.resolve(rows(1, 2));
  await hydrate;
  assertEquals(transcript.rows.length, 2);

  // Two polls race on the same tail: the newer pull owns the write, so the
  // rows land once instead of twice.
  const older = transcript.refresh();
  const newer = transcript.refresh();
  assertEquals(asked, [0, 2, 2]);
  gates[1]!.resolve(rows(3, 2));
  gates[2]!.resolve(rows(3, 2));
  await older;
  await newer;

  assertEquals(
    transcript.rows.map((r) => r.seq),
    [1, 2, 3, 4],
  );
  assertEquals(transcript.lastSeq, 4);
});

Deno.test("an answered-empty transcript walks the tail next time", async () => {
  const asked: number[] = [];
  const transcript = createDirectTranscript({
    pageSize: 2,
    maxPages: 3,
    fetchPage: (_tagma, _peer, afterSeq) => {
      asked.push(afterSeq);
      // Nothing at all on the first read; a full page later, which a hydrate
      // would follow with another request and an increment would not.
      return Promise.resolve(asked.length === 1 ? [] : rows(1, 2));
    },
  });

  transcript.open("t1", "p1");
  await transcript.refresh();

  // An empty page is an answer: the pair is ready, not unloaded.
  assertEquals(transcript.phase, "ready");
  assertEquals(transcript.rows, []);
  assertEquals(asked, [0]);

  await transcript.refresh();
  assertEquals(asked, [0, 0]);
  assertEquals(
    transcript.rows.map((r) => r.seq),
    [1, 2],
  );
});
