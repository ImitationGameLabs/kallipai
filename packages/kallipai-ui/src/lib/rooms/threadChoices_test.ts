// The standing cache: one read per thread, one shared request while a read is
// out, a failure remembered instead of retried, and a write replacing what the
// read left. The route is stubbed at the wire, and the passthrough
// `$state` shim lets the rune-bearing module run uncompiled -- the harness the
// other store tests use.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const { withFetch } = await import("@kallipai/kallipai-common/testing");
const { initRelays } = await import("../session/archeion.svelte.ts");
const {
  clearThreadChoice,
  ensureThreadChoice,
  setThreadChoice,
  threadChoiceOf,
  THREAD_CHOICES_MAX,
  UNKNOWN_RETRY_MS,
} = await import("./threadChoices.svelte.ts");

interface View {
  readonly override: "following" | "muted" | null;
  readonly effective: "following" | "muted" | "involved";
  readonly since_seq: number | null;
}

function view(override: View["override"], since = 9): View {
  return {
    override,
    effective: override ?? "following",
    since_seq: override === null ? null : since,
  };
}

// The subscription route, stubbed at the wire: `nextRead` decides what one read
// answers (or rejects with), and each request is recorded by method before it
// answers. A rejection stands for a request that never landed, the same shape
// the real relay produces.
let reads: [string, number][] = [];
let writes: [string, number, string][] = [];
let clears: [string, number][] = [];
let nextRead: () => Promise<View> = () => Promise.resolve(view("muted"));

initRelays("http://example.invalid", "http://example.invalid");

/** `/rooms/{id}/threads/{root}/subscription` split into its two arguments, or
 * null for any other path (the stub answers 404, so a renamed route fails). */
function subscriptionOf(path: string): { roomId: string; root: number } | null {
  const parts = path.split("/");
  if (
    parts.length !== 6 ||
    parts[0] !== "" ||
    parts[1] !== "rooms" ||
    parts[3] !== "threads" ||
    parts[5] !== "subscription"
  ) {
    return null;
  }
  return { roomId: decodeURIComponent(parts[2]!), root: Number(parts[4]) };
}
function jsonAnswer(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}
const stub = (async (input: string | URL, init?: RequestInit) => {
  const url = new URL(String(input));
  const route = subscriptionOf(url.pathname);
  if (route === null) return jsonAnswer({ error: "no such route" }, 404);
  const { roomId, root } = route;
  const method = init?.method ?? "GET";
  if (method === "GET") {
    reads.push([roomId, root]);
    return jsonAnswer(await nextRead());
  }
  if (method === "PUT") {
    const body = JSON.parse(String(init?.body ?? "{}")) as { state: string };
    writes.push([roomId, root, body.state]);
    return jsonAnswer(view(body.state as View["override"]));
  }
  if (method === "DELETE") {
    clears.push([roomId, root]);
    return new Response(null, { status: 204 });
  }
  return jsonAnswer({ error: { message: "unexpected method" } }, 405);
}) as typeof fetch;
/** Every case runs against the stub. */
function test(name: string, run: () => Promise<void>): void {
  Deno.test(name, () => withFetch(stub, run));
}
function reset(): void {
  reads = [];
  writes = [];
  clears = [];
  nextRead = () => Promise.resolve(view("muted"));
}

test("two draws of one thread share one request", async () => {
  reset();
  const first = await ensureThreadChoice("room-a", 5);
  const second = await ensureThreadChoice("room-a", 5);
  assertEquals(reads, [["room-a", 5]]);
  assertEquals(first, second);
  assertEquals(threadChoiceOf("room-a", 5), view("muted"));
});

test("a failed read is not retried inside the window", async () => {
  reset();
  nextRead = () => Promise.reject(new Error("offline"));
  const t0 = 1_000_000;
  assertEquals(await ensureThreadChoice("room-b", 7, () => t0), null);
  assertEquals(await ensureThreadChoice("room-b", 7, () => t0 + 1), null);
  assertEquals(reads, [["room-b", 7]]);
  // Unknown is not a standing: a surface draws nothing rather than a guess.
  assertEquals(threadChoiceOf("room-b", 7), null);
});

test("a failed read is asked again once the window passes", async () => {
  // One blip must not cost the thread its mark and its control for the rest of
  // the session, so the next draw after the window asks once more.
  reset();
  nextRead = () => Promise.reject(new Error("offline"));
  const t0 = 2_000_000;
  await ensureThreadChoice("room-f", 9, () => t0);
  await ensureThreadChoice("room-f", 9, () => t0 + UNKNOWN_RETRY_MS - 1);
  assertEquals(reads.length, 1);
  await ensureThreadChoice("room-f", 9, () => t0 + UNKNOWN_RETRY_MS);
  assertEquals(reads.length, 2);
});

test("a retry that answers replaces the unknown", async () => {
  reset();
  nextRead = () => Promise.reject(new Error("offline"));
  const t0 = 3_000_000;
  await ensureThreadChoice("room-g", 4, () => t0);
  nextRead = () => Promise.resolve(view("muted"));
  const standing = await ensureThreadChoice(
    "room-g",
    4,
    () => t0 + UNKNOWN_RETRY_MS,
  );
  assertEquals(standing, view("muted"));
  assertEquals(threadChoiceOf("room-g", 4), view("muted"));
});

test("the retry window runs from the answer, not the request", async () => {
  // A slow failure must not shorten its own silence: the window opens when the
  // answer lands, not when the request went out.
  reset();
  const t0 = 4_000_000;
  let t = t0;
  nextRead = () => {
    t = t0 + 25_000;
    return Promise.reject(new Error("offline"));
  };
  assertEquals(await ensureThreadChoice("room-h", 6, () => t), null);
  assertEquals(
    await ensureThreadChoice(
      "room-h",
      6,
      () => t0 + 25_000 + UNKNOWN_RETRY_MS - 1,
    ),
    null,
  );
  assertEquals(reads, [["room-h", 6]]);
  await ensureThreadChoice("room-h", 6, () => t0 + 25_000 + UNKNOWN_RETRY_MS);
  assertEquals(reads.length, 2);
});

test("a write replaces the standing the read left", async () => {
  reset();
  await ensureThreadChoice("room-c", 2);
  const written = await setThreadChoice("room-c", 2, "following");
  assertEquals(writes, [["room-c", 2, "following"]]);
  assertEquals(written, view("following"));
  assertEquals(threadChoiceOf("room-c", 2), view("following"));
});

test("dropping a choice reads the default back", async () => {
  reset();
  nextRead = () => Promise.resolve(view(null));
  const standing = await clearThreadChoice("room-d", 3);
  assertEquals(clears, [["room-d", 3]]);
  assertEquals(reads, [["room-d", 3]]);
  // What a dropped choice lands on belongs to the caller's identity, so the
  // route's answer is what the control shows.
  assertEquals(standing, view(null));
  assertEquals(threadChoiceOf("room-d", 3), view(null));
});

test("the oldest standing leaves first", async () => {
  reset();
  await ensureThreadChoice("room-oldest", 1);
  for (let root = 1; root <= THREAD_CHOICES_MAX; root += 1) {
    await ensureThreadChoice("room-e", root);
  }
  assertEquals(threadChoiceOf("room-oldest", 1), null);
  assertEquals(threadChoiceOf("room-e", THREAD_CHOICES_MAX), view("muted"));
});
