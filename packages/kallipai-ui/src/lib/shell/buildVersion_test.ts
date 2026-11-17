// The build-version watch: which ids it believes, which it ignores, when one
// answer ends the asking for good, and when the next ask is worth making.

import { assert, assertEquals } from "@std/assert";
import {
  answerFor,
  fetchVersionAnswer,
  startVersionWatch,
  usableVersionId,
  type VersionAnswer,
  type VersionDocument,
  type VersionFetch,
  type VersionResponse,
} from "./buildVersion.ts";

/** Let the watch's fetch and judgment settle. */
const settle = () => new Promise((resolve) => setTimeout(resolve, 5));

/** An answer carrying a candidate the watch still has to weigh. */
const idAnswer = (candidate: unknown): VersionAnswer => ({
  kind: "id",
  candidate,
});

/** The answer of a deployment that publishes no version file at all. */
const ABSENT: VersionAnswer = { kind: "absent" };

/** A document stand-in: listeners land here and the test says when the page
 *  is visible again. */
function fakeDocument(visibility: "visible" | "hidden" = "visible") {
  const listeners = new Set<() => void>();
  const doc: VersionDocument = {
    get visibilityState() {
      return visibility;
    },
    addEventListener: (_: string, fn: () => void) => listeners.add(fn),
    removeEventListener: (_: string, fn: () => void) => listeners.delete(fn),
  };
  return {
    doc,
    /** Flip visibility and fire the event, as the browser would. */
    become(next: "visible" | "hidden") {
      visibility = next;
      for (const fn of listeners) fn();
    },
    listenerCount: () => listeners.size,
  };
}

Deno.test("buildVersion: a different id reports once", async () => {
  const { doc } = fakeDocument();
  let announced = 0;
  let asks = 0;
  const stop = startVersionWatch({
    current: "1700000000000",
    fetchVersion: () => {
      asks += 1;
      return Promise.resolve(idAnswer("1800000000000"));
    },
    onAvailable: () => (announced += 1),
    intervalMs: 2,
    doc,
  });
  await settle();
  await settle();
  stop();
  assertEquals(announced, 1, "one notice");
  assertEquals(asks, 1, "the answer ended the asking");
});

Deno.test("buildVersion: the running id is not news", async () => {
  const { doc } = fakeDocument();
  let announced = 0;
  const stop = startVersionWatch({
    current: "1700000000000",
    fetchVersion: () => Promise.resolve(idAnswer("1700000000000")),
    onAvailable: () => (announced += 1),
    intervalMs: 2,
    doc,
  });
  await settle();
  await settle();
  stop();
  assertEquals(announced, 0);
});

Deno.test(
  "buildVersion: an answer that is not an id ends the watch",
  async () => {
    // The field arrived, so the deployment did answer -- with something that
    // is not a version. The same build asked again returns the same bytes.
    for (const value of ["", "   ", "x".repeat(65), null, undefined, 42]) {
      const { doc, listenerCount } = fakeDocument();
      let announced = 0;
      let asks = 0;
      const stop = startVersionWatch({
        current: "1700000000000",
        fetchVersion: () => {
          asks += 1;
          return Promise.resolve(idAnswer(value));
        },
        onAvailable: () => (announced += 1),
        intervalMs: 2,
        doc,
      });
      await settle();
      await settle();
      assertEquals(announced, 0, `no notice for ${JSON.stringify(value)}`);
      assertEquals(asks, 1, `one ask for ${JSON.stringify(value)}`);
      assertEquals(
        listenerCount(),
        0,
        `the watch stopped itself on ${JSON.stringify(value)}`,
      );
      stop();
    }
  },
);

Deno.test(
  "buildVersion: a deployment that publishes no id is asked once",
  async () => {
    // The file is a build artifact: its absence is a fact about the
    // deployment, so the second ask would ask the same build the same
    // question and get the same answer.
    const { doc, listenerCount } = fakeDocument();
    let announced = 0;
    let asks = 0;
    const stop = startVersionWatch({
      current: "1700000000000",
      fetchVersion: () => {
        asks += 1;
        return Promise.resolve(ABSENT);
      },
      onAvailable: () => (announced += 1),
      intervalMs: 2,
      doc,
    });
    await settle();
    await settle();
    await settle();
    assertEquals(listenerCount(), 0, "the watch stopped itself");
    assertEquals(announced, 0, "an absent answer is not news");
    stop();
    assertEquals(asks, 1, "one ask, then silence");
  },
);

Deno.test(
  "buildVersion: a rollback below the running id still reports",
  async () => {
    // The served id is older than the running one: a page left on a superseded
    // bundle is exactly the page that should be told, so the judgment is
    // "different", not "newer".
    const { doc } = fakeDocument();
    let announced = 0;
    const stop = startVersionWatch({
      current: "2026-11-07-b",
      fetchVersion: () => Promise.resolve(idAnswer("2026-11-06-a")),
      onAvailable: () => (announced += 1),
      intervalMs: 60_000,
      doc,
    });
    await settle();
    stop();
    assertEquals(announced, 1);
  },
);

Deno.test(
  "buildVersion: a failed ask is retried on the next interval",
  async () => {
    const { doc } = fakeDocument();
    let announced = 0;
    let asks = 0;
    const stop = startVersionWatch({
      current: "1700000000000",
      fetchVersion: () => {
        asks += 1;
        return asks === 1
          ? Promise.reject(new Error("offline"))
          : Promise.resolve(idAnswer("1800000000000"));
      },
      onAvailable: () => (announced += 1),
      intervalMs: 2,
      doc,
    });
    await settle();
    await settle();
    stop();
    assertEquals(announced, 1, "the second ask reports");
  },
);

Deno.test("buildVersion: after stop the interval asks nothing", async () => {
  // The running id keeps the watch asking, so the ask after `stop` would show.
  const { doc } = fakeDocument();
  let asks = 0;
  const stop = startVersionWatch({
    current: "1700000000000",
    fetchVersion: () => {
      asks += 1;
      return Promise.resolve(idAnswer("1700000000000"));
    },
    onAvailable: () => {},
    intervalMs: 1,
    doc,
  });
  await settle();
  const asked = asks;
  assert(asked > 0, "the watch asked while it ran");
  stop();
  await settle();
  await settle();
  assertEquals(asks, asked, "no ask follows the stop");
});

Deno.test(
  "buildVersion: stop before the answer silences the notice",
  async () => {
    const { doc, listenerCount } = fakeDocument();
    let announced = 0;
    let resolve: (value: VersionAnswer) => void = () => {};
    const pending = new Promise<VersionAnswer>((r) => (resolve = r));
    const stop = startVersionWatch({
      current: "1700000000000",
      fetchVersion: () => pending,
      onAvailable: () => (announced += 1),
      intervalMs: 60_000,
      doc,
    });
    stop();
    resolve(idAnswer("1800000000000"));
    await settle();
    assertEquals(announced, 0);
    assertEquals(listenerCount(), 0, "the visibility hook is gone");
  },
);

Deno.test("buildVersion: returning to the page asks again", async () => {
  const { doc, become } = fakeDocument("hidden");
  let announced = 0;
  let asks = 0;
  const stop = startVersionWatch({
    current: "1700000000000",
    fetchVersion: () => {
      asks += 1;
      return asks === 1
        ? Promise.reject(new Error("offline"))
        : Promise.resolve(idAnswer("1800000000000"));
    },
    onAvailable: () => (announced += 1),
    intervalMs: 60_000,
    doc,
  });
  await settle();
  assertEquals(announced, 0, "the first ask failed");
  become("visible");
  await settle();
  stop();
  assertEquals(announced, 1);
});

Deno.test(
  "buildVersion: only a bounded, whitespace-free string is an id",
  () => {
    assertEquals(usableVersionId("abc123"), true);
    assertEquals(usableVersionId("x".repeat(64)), true);
    assertEquals(usableVersionId(""), false);
    assertEquals(usableVersionId("x".repeat(65)), false);
    assertEquals(usableVersionId("a b"), false);
    assertEquals(usableVersionId(null), false);
    assertEquals(usableVersionId(7), false);
  },
);

/** A stand-in response: one ask reads `status`, `ok`, and the text body. */
function fakeResponse(
  status: number,
  body: string,
  options: { textError?: Error } = {},
): VersionResponse {
  return {
    status,
    ok: status >= 200 && status < 300,
    text: () =>
      options.textError === undefined
        ? Promise.resolve(body)
        : Promise.reject(options.textError),
  };
}

/** A real `Response`, so the two failure shapes below pin what the engine
 *  does rather than what this test remembers about it. */
function realResponse(body: BodyInit | null): VersionResponse {
  return new Response(body, { status: 200 });
}

Deno.test("buildVersion: a missing version file is absent", async () => {
  for (const status of [404, 410]) {
    assertEquals(await answerFor(fakeResponse(status, "")), ABSENT);
  }
});

Deno.test(
  "buildVersion: a body that is not the version document is absent",
  async () => {
    // `compose/site.caddy` answers `try_files {path} /index.html`, so a file
    // the build lacks comes back as the shell with a 200; a cache can serve
    // the same shape for a deployment that publishes nothing.
    assertEquals(await answerFor(realResponse("<!doctype html>")), ABSENT);
    assertEquals(await answerFor(fakeResponse(200, "{}")), ABSENT);
    assertEquals(await answerFor(fakeResponse(200, '{"version":7}')), ABSENT);
    assertEquals(await answerFor(fakeResponse(200, "")), ABSENT);
  },
);

Deno.test(
  "buildVersion: a body that stops mid-read stays undecided",
  async () => {
    // The headers arrived, so the status says the file is there; the bytes
    // ran out. That is the same kind of moment as a dropped connection, not
    // an answer about the build.
    const broken = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.error(new Error("connection reset"));
      },
    });
    let threw = false;
    try {
      await answerFor(realResponse(broken));
    } catch {
      threw = true;
    }
    assertEquals(threw, true);
  },
);

Deno.test("buildVersion: a server failure is not an answer", async () => {
  for (const status of [500, 502, 503]) {
    let threw = false;
    try {
      await answerFor(fakeResponse(status, ""));
    } catch {
      threw = true;
    }
    assertEquals(threw, true, `${status} leaves the answer undecided`);
  }
});

Deno.test(
  "buildVersion: a published document carries its version",
  async () => {
    assertEquals(
      await answerFor(fakeResponse(200, '{"version":"1800000000000"}')),
      idAnswer("1800000000000"),
    );
    // Reading the field is all the ask does: whether the value is believable
    // is the watch's call, so an empty one arrives as a candidate too.
    assertEquals(
      await answerFor(fakeResponse(200, '{"version":""}')),
      idAnswer(""),
    );
  },
);

Deno.test(
  "buildVersion: the ask names the file and its cache breaker",
  async () => {
    const seen: string[] = [];
    const fetchImpl: VersionFetch = (url) => {
      seen.push(url);
      return Promise.resolve(fakeResponse(200, '{"version":"1700000000000"}'));
    };
    assertEquals(
      await fetchVersionAnswer("/app", fetchImpl, () => 1234),
      idAnswer("1700000000000"),
    );
    assertEquals(seen, ["/app/_app/version.json?t=1234"]);
  },
);

Deno.test(
  "buildVersion: a dropped connection reaches the watch as a throw",
  async () => {
    let threw = false;
    try {
      await fetchVersionAnswer(
        "/",
        () => Promise.reject(new Error("offline")),
        () => 1,
      );
    } catch {
      threw = true;
    }
    assertEquals(threw, true);
  },
);
