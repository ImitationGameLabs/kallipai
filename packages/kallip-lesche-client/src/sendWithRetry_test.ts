// Tests for the network-layer send retry (operator-final params: 5 attempts
// total, exponential 1/2/4/8s backoff) and the pending-row failed partition
// the reconnect flush uses. Both units are pure: the retry's sleep is
// injected (no real waiting), and isFailedPending touches no IndexedDB.

import { assert, assertEquals, assertRejects } from "@std/assert";
import { LescheApiError } from "./types.ts";
import { isRetryableSendError, sendWithRetry } from "./channel.ts";
import { isFailedPending, type PendingLine } from "./cache.ts";

const instantSleep = () => Promise.resolve();

Deno.test("sendWithRetry succeeds after transient failures", async () => {
  const delays: number[] = [];
  let attempts = 0;
  const result = await sendWithRetry(
    () => {
      attempts += 1;
      if (attempts < 3) {
        return Promise.reject(new TypeError("network down"));
      }
      return Promise.resolve("ok");
    },
    [10, 20, 40],
    (ms) => {
      delays.push(ms);
      return instantSleep();
    },
  );
  assertEquals(result, "ok");
  assertEquals(attempts, 3);
  assertEquals(delays.length, 2);
});

Deno.test(
  "sendWithRetry exhausts attempts and throws the last error",
  async () => {
    let attempts = 0;
    const error = await assertRejects(
      () =>
        sendWithRetry(
          () => {
            attempts += 1;
            return Promise.reject(new TypeError("still down"));
          },
          [10, 20],
          instantSleep,
        ),
      TypeError,
    );
    assert(error instanceof TypeError);
    assertEquals(attempts, 3); // first try + 2 retries
  },
);

Deno.test("sendWithRetry never retries a client error", async () => {
  let attempts = 0;
  await assertRejects(
    () =>
      sendWithRetry(
        () => {
          attempts += 1;
          return Promise.reject(
            new LescheApiError(409, "profile_set_unusable"),
          );
        },
        [10, 20],
        instantSleep,
      ),
    LescheApiError,
  );
  assertEquals(attempts, 1);
});

Deno.test("isRetryableSendError classifies by status family", () => {
  assert(isRetryableSendError(new TypeError("fetch failed")));
  assert(isRetryableSendError(new LescheApiError(503, "tagma offline")));
  assert(!isRetryableSendError(new LescheApiError(400, "bad envelope")));
  assert(!isRetryableSendError(new LescheApiError(409, "dangling binding")));
});

function row(overrides: Partial<PendingLine>): PendingLine {
  return {
    tagmaId: "t1",
    localSeq: -1,
    text: "hello",
    createdAt: "2026-09-23T00:00:00Z",
    ...overrides,
  };
}

Deno.test(
  "isFailedPending partitions explicit, legacy, and queued rows",
  () => {
    // Explicit failed status.
    assert(isFailedPending(row({ status: "failed", lastError: "boom" })));
    // Legacy shape: pre-status rows with a stored error count as failed.
    assert(isFailedPending(row({ lastError: "boom" })));
    // Queued (never sent, no error) does not count as failed...
    assert(!isFailedPending(row({ status: "queued" })));
    // ...including the legacy shape with no error at all.
    assert(!isFailedPending(row({})));
  },
);

Deno.test("isFailedPending locks the auto-resend direction", () => {
  const queued = row({ localSeq: -3, status: "queued" });
  const queuedExplicit = row({ localSeq: -4, status: "queued" });
  const failed = row({ localSeq: -1, status: "failed", lastError: "boom" });
  const failedLegacy = row({ localSeq: -2, lastError: "boom" });
  // Queued rows are exactly the auto-send set (distinct ids make the
  // direction of the split observable)...
  assert(!isFailedPending(queued));
  assert(!isFailedPending(queuedExplicit));
  // ...and failed rows are exactly the wait-for-retry set: flipping this
  // predicate would auto-resend them.
  assert(isFailedPending(failed));
  assert(isFailedPending(failedLegacy));
});
