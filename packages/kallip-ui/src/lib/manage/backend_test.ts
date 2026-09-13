// The online shell's confirm flow keys off the structured dangling list a
// 409 profiles save carries. These tests pin that the relay path
// reconstructs the full KallipError — not a flattened message-only one —
// and that the save-failure classification still routes a bare 409 (old
// backend) to the stale-backend branch.

import { assertEquals } from "@std/assert";
import { KallipError } from "@kallipai/kallip-common";
import {
  type ManageRestClient,
  ProjectionClient,
} from "@kallipai/kallip-lesche-client";
import { OnlineBackend } from "./backend.ts";

// The projection feed consults document.hidden and binds a
// visibilitychange listener at backend construction; deno test has
// no DOM, so stand in a minimal visible document unless a test
// installs its own.
if (typeof globalThis.document === "undefined") {
  globalThis.document = {
    hidden: false,
    addEventListener: () => {},
    removeEventListener: () => {},
  } as unknown as typeof globalThis.document;
}
import { classifySaveFailure } from "./profiles-view.ts";
function restWith(body: unknown): ManageRestClient {
  return {
    manage: () => Promise.resolve({ status: 409, body }),
  } as unknown as ManageRestClient;
}

const PUT_BODY = {
  endpoints: {},
  sets: [],
  parking: [],
  default: "",
};

const DANGLING_BODY = {
  error: {
    message: "config drops sets still bound by agents: agent-x → 'alt'",
    dangling: ["agent-x → 'alt'"],
  },
};

Deno.test(
  "a relayed 409 with a dangling list reaches the confirm flow",
  async () => {
    const backend = new OnlineBackend(restWith(DANGLING_BODY), "t-a");
    let caught: unknown = null;
    await backend.updateProfiles({ ...PUT_BODY, force: false }).catch((e) => {
      caught = e;
    });
    assertEquals(caught instanceof KallipError, true);
    const err = caught as KallipError;
    assertEquals(err.api.status, 409);
    assertEquals(err.api.dangling, ["agent-x → 'alt'"]);
    // The store's catch classifies this as the confirm-flow branch.
    assertEquals(classifySaveFailure(err, false), "park-dangling");
  },
);

Deno.test(
  "a bare relayed 409 (old backend) still lands on the downgrade branch",
  async () => {
    const backend = new OnlineBackend(
      restWith({ error: { message: "config drops sets still bound" } }),
      "t-a",
    );
    let caught: unknown = null;
    await backend.updateProfiles({ ...PUT_BODY, force: true }).catch((e) => {
      caught = e;
    });
    assertEquals(caught instanceof KallipError, true);
    const err = caught as KallipError;
    assertEquals(err.api.status, 409);
    assertEquals(err.api.dangling, undefined);
    // With force and no structured list: the old-backend downgrade.
    assertEquals(classifySaveFailure(err, true), "stale-backend");
  },
);

Deno.test(
  "a plain-text relayed 403 folds into the same KallipError shape",
  async () => {
    // The proxy answers forbidden agents with a bare text body; the
    // string branch must yield the same KallipError shape the envelope
    // path produces.
    const rest = {
      manage: () => Promise.resolve({ status: 403, body: "not your tagma" }),
    } as unknown as ManageRestClient;
    const backend = new OnlineBackend(rest, "t-a");
    let caught: unknown = null;
    await backend.getBudget().catch((e) => {
      caught = e;
    });
    assertEquals(caught instanceof KallipError, true);
    const err = caught as KallipError;
    assertEquals(err.api.status, 403);
    assertEquals(err.api.message, "not your tagma");
  },
);

Deno.test(
  "the agent id is URL-encoded in the relayed manage path",
  async () => {
    // The backend path builders embed the agent id directly; pin that
    // what reaches the rest client is the encoded form, not the raw
    // id (a stray "/" or "?" would warp the frame path).
    const captured: Array<{ path: string }> = [];
    const rest = {
      manage: (_agent: string, _method: string, path: string) => {
        captured.push({ path });
        return Promise.resolve({ status: 200, body: {} });
      },
    } as unknown as ManageRestClient;
    const backend = new OnlineBackend(rest, "t-a");
    await backend.getAgentStatus("a/b c");
    assertEquals(captured.length, 1);
    assertEquals(captured[0]!.path, "/agents/a%2Fb%20c/status");
  },
);

// --- the projection seam -------------------------------------------

// End-to-end across the seam: listAgents rides the projection GET, and a
// dirty frame pumped through the stubbed SSE stream reaches the feed
// subscriber -- the exact chain a real lesche drives (store -> SSE -> GET).
Deno.test(
  "projection-backed listAgents and a dirty frame reach the feed",
  async () => {
    const sse = 'data: {"tagma_id":"t-a","seq":9}\n\n';
    const seen: string[] = [];
    const real = globalThis.fetch;
    globalThis.fetch = ((url: string | URL | Request) => {
      const u = String(url);
      seen.push(u);
      if (u.endsWith("/v1/lesche/tagmata/t-a/agents")) {
        return Promise.resolve(
          Response.json({
            stale: false,
            seq: 9,
            updated_at: 1,
            agents: [
              {
                id: "root",
                workspace_root: "/w",
                state: "idle",
                created_by: null,
                role: "",
                description: "",
                activity: "",
                duty: "onduty",
                faulted_reason: null,
                conversation_id: null,
              },
            ],
            status: {},
          }),
        );
      }
      return Promise.resolve(
        new Response(sse, {
          status: 200,
          headers: { "content-type": "text/event-stream" },
        }),
      );
    }) as typeof fetch;
    try {
      const backend = new OnlineBackend(
        {} as unknown as ManageRestClient, // the manage relay stays untouched
        "t-a",
        new ProjectionClient("http://lesche.test/v1/lesche"),
      );
      const agents = await backend.listAgents();
      assertEquals(agents.agents[0]!.id, "root");
      assertEquals(
        seen.some((u) => u.endsWith("/v1/lesche/tagmata/t-a/agents")),
        true,
      );

      // The feed: one dirty nudge reaches the subscriber, no polling.
      let nudges = 0;
      const stop = backend.projectionFeed!.subscribe(() => {
        nudges += 1;
      });
      await new Promise((r) => setTimeout(r, 60));
      stop();
      assertEquals(nudges >= 1, true);
    } finally {
      globalThis.fetch = real;
    }
  },
);

// Regression (review round): the stop handle must only retire its own
// callback -- a sibling keeps receiving nudges, and a fresh subscribe
// after everyone left revives the loop. Frames are timed across the
// 750ms shaping window so each one is a fresh, uncollapsed nudge.
Deno.test(
  "stopping one subscriber leaves siblings and revival intact",
  async () => {
    const enc = new TextEncoder();
    const frame = (seq: number) =>
      enc.encode('data: {"tagma_id":"t-a","seq":' + seq + "}\n\n");
    const real = globalThis.fetch;
    globalThis.fetch = (() =>
      Promise.resolve(
        new Response(
          new ReadableStream<Uint8Array>({
            start(controller) {
              controller.enqueue(frame(1));
              // Later frames ride timers; if the stream is torn down by
              // an unsubscribe first, the enqueue throws and is swallowed.
              setTimeout(() => {
                try {
                  controller.enqueue(frame(2));
                } catch {
                  // stream torn down by an unsubscribe
                }
              }, 850);
              setTimeout(() => {
                try {
                  controller.enqueue(frame(3));
                } catch {
                  // stream torn down by an unsubscribe
                }
              }, 1700);
            },
          }),
          { status: 200, headers: { "content-type": "text/event-stream" } },
        ),
      )) as typeof fetch;
    try {
      const backend = new OnlineBackend(
        {} as unknown as ManageRestClient,
        "t-a",
        new ProjectionClient("http://lesche.test"),
      );
      let aCount = 0;
      let bCount = 0;
      const stopA = backend.projectionFeed!.subscribe(() => {
        aCount += 1;
      });
      const stopB = backend.projectionFeed!.subscribe(() => {
        bCount += 1;
      });
      // seq 1 notifies both subscribers immediately.
      await new Promise((r) => setTimeout(r, 60));
      assertEquals(aCount, 1);
      assertEquals(bCount, 1);
      stopA();
      // seq 2 arrives past the shaping window: only the sibling hears
      // it; the stopped callback stays retired.
      await new Promise((r) => setTimeout(r, 900));
      assertEquals(aCount, 1, "stopped subscriber stays stopped");
      assertEquals(bCount, 2, "sibling keeps receiving");

      // Everyone unsubscribes (the loop aborts), then a fresh
      // subscriber revives it with a new stream; the first frame past
      // the seq gate reaches it.
      stopB();
      let cCount = 0;
      backend.projectionFeed!.subscribe(() => {
        cCount += 1;
      });
      await new Promise((r) => setTimeout(r, 1800));
      assertEquals(cCount >= 1, true, "fresh subscribe revives the loop");
    } finally {
      globalThis.fetch = real;
    }
  },
);
// Regression (review round): dirty-frame traffic shaping. A burst of
// frames inside the 750ms window folds into one trailing catch-up
// nudge at the window tail, and a frame at or below the last
// notified seq is a replay, not news.
Deno.test(
  "a frame burst folds into one trailing nudge; stale seq frames drop",
  async () => {
    const sse =
      'data: {"tagma_id":"t-a","seq":5}\n\n' +
      'data: {"tagma_id":"t-a","seq":6}\n\n' +
      'data: {"tagma_id":"t-a","seq":5}\n\n';
    const real = globalThis.fetch;
    let call = 0;
    globalThis.fetch = (() => {
      call += 1;
      if (call > 1) {
        // Later connections idle open: a replayed short stream would
        // re-notify by design (per-connection seq reset), which is not
        // this test's subject.
        return Promise.resolve(
          new Response(new ReadableStream<Uint8Array>({ start() {} }), {
            status: 200,
            headers: { "content-type": "text/event-stream" },
          }),
        );
      }
      return Promise.resolve(
        new Response(sse, {
          status: 200,
          headers: { "content-type": "text/event-stream" },
        }),
      );
    }) as typeof fetch;
    try {
      const backend = new OnlineBackend(
        {} as unknown as ManageRestClient,
        "t-a",
        new ProjectionClient("http://lesche.test"),
      );
      let nudges = 0;
      const stop = backend.projectionFeed!.subscribe(() => {
        nudges += 1;
      });
      // seq 5 fires immediately (leading edge); seq 6 folds into the
      // window and the tail fires one catch-up nudge; the trailing
      // seq 5 sits at the notified seq and drops.
      await new Promise((r) => setTimeout(r, 60));
      assertEquals(nudges, 1, "the leading nudge only, burst absorbed");
      await new Promise((r) => setTimeout(r, 800));
      assertEquals(nudges, 2, "the window tail fires one catch-up");
      stop();
    } finally {
      globalThis.fetch = real;
    }
  },
);
// Regression (review round): frames arriving while the page is hidden
// must not notify; they stay pending and flush exactly once when the
// page turns visible again.
Deno.test(
  "frames seen while hidden stay pending until the page turns visible",
  async () => {
    let visListener: (() => void) | null = null;
    const doc = {
      hidden: true,
      addEventListener: (_type: string, fn: () => void) => {
        visListener = fn;
      },
      removeEventListener: () => {},
    };
    const real = globalThis.fetch;
    const realDoc = globalThis.document;
    let call = 0;
    globalThis.fetch = (() => {
      call += 1;
      if (call > 1) {
        // Later connections idle open so the replayed frame does not
        // muddy the pending-flush assertions.
        return Promise.resolve(
          new Response(new ReadableStream<Uint8Array>({ start() {} }), {
            status: 200,
            headers: { "content-type": "text/event-stream" },
          }),
        );
      }
      return Promise.resolve(
        new Response('data: {"tagma_id":"t-a","seq":3}\n\n', {
          status: 200,
          headers: { "content-type": "text/event-stream" },
        }),
      );
    }) as typeof fetch;
    globalThis.document = doc as unknown as typeof globalThis.document;
    try {
      const backend = new OnlineBackend(
        {} as unknown as ManageRestClient,
        "t-a",
        new ProjectionClient("http://lesche.test"),
      );
      let nudges = 0;
      const stop = backend.projectionFeed!.subscribe(() => {
        nudges += 1;
      });
      await new Promise((r) => setTimeout(r, 60));
      assertEquals(nudges, 0, "hidden swallows the frame");
      doc.hidden = false;
      visListener!();
      await new Promise((r) => setTimeout(r, 10));
      assertEquals(nudges, 1, "the flip flushes the pending nudge");
      visListener!();
      await new Promise((r) => setTimeout(r, 10));
      assertEquals(nudges, 1, "a flip with nothing pending is inert");
      stop();
    } finally {
      globalThis.fetch = real;
      globalThis.document = realDoc;
    }
  },
);

// Regression (review round): a lesche restart renumbers the stream --
// seq starts back at 1. The seq gate resets per connection, so the
// renumbered stream still notifies instead of reading as replays.
Deno.test("a renumbered stream still notifies after a reconnect", async () => {
  let stream = 0;
  const real = globalThis.fetch;
  globalThis.fetch = (() => {
    stream += 1;
    if (stream > 2) {
      // Third and later connections idle open: an endless chain of
      // one-frame streams would re-notify forever.
      return Promise.resolve(
        new Response(new ReadableStream<Uint8Array>({ start() {} }), {
          status: 200,
          headers: { "content-type": "text/event-stream" },
        }),
      );
    }
    const seq = stream === 1 ? 9 : 1;
    return Promise.resolve(
      new Response('data: {"tagma_id":"t-a","seq":' + seq + "}\n\n", {
        status: 200,
        headers: { "content-type": "text/event-stream" },
      }),
    );
  }) as typeof fetch;
  try {
    const backend = new OnlineBackend(
      {} as unknown as ManageRestClient,
      "t-a",
      new ProjectionClient("http://lesche.test"),
    );
    let nudges = 0;
    const stop = backend.projectionFeed!.subscribe(() => {
      nudges += 1;
    });
    // Stream one: seq 9 notifies (leading). It ends, the backoff is
    // zero after a clean frame, and stream two opens renumbered at
    // seq 1; its frame folds into the window and the tail fires.
    await new Promise((r) => setTimeout(r, 900));
    assertEquals(
      nudges,
      2,
      "old stream leading + renumbered stream catch-up both notify",
    );
    stop();
  } finally {
    globalThis.fetch = real;
  }
});

// Regression (review round): the last unsubscribe detaches the
// document-level visibilitychange listener; a revived feed must
// re-arm it, or frames seen while hidden would pend with no one
// left to flush them.
Deno.test(
  "revival re-arms the visibility flush after full detach",
  async () => {
    // A real listener set: add/remove mutate it, dispatch walks
    // it, so a detached listener is observably gone.
    const listeners = new Set<() => void>();
    const doc = {
      hidden: false,
      addEventListener: (_type: string, fn: () => void) => {
        listeners.add(fn);
      },
      removeEventListener: (_type: string, fn: () => void) => {
        listeners.delete(fn);
      },
    };
    const dispatch = (): void => {
      for (const fn of [...listeners]) fn();
    };
    let stream = 0;
    const real = globalThis.fetch;
    const realDoc = globalThis.document;
    globalThis.fetch = (() => {
      stream += 1;
      if (stream === 1) {
        // First stint idles open: the pre-detach subscription
        // sees no frames, keeping the subject on the revival
        // connection.
        return Promise.resolve(
          new Response(new ReadableStream<Uint8Array>({ start() {} }), {
            status: 200,
            headers: { "content-type": "text/event-stream" },
          }),
        );
      }
      if (stream === 2) {
        // Revival connection: a visible frame (leading nudge)
        // then a hidden one (pends behind pendingWhileHidden).
        const encoder = new TextEncoder();
        return Promise.resolve(
          new Response(
            new ReadableStream<Uint8Array>({
              start(controller) {
                controller.enqueue(
                  encoder.encode('data: {"tagma_id":"t-a","seq":5}\n\n'),
                );
                setTimeout(() => {
                  controller.enqueue(
                    encoder.encode('data: {"tagma_id":"t-a","seq":6}\n\n'),
                  );
                }, 150);
              },
            }),
            {
              status: 200,
              headers: { "content-type": "text/event-stream" },
            },
          ),
        );
      }
      // Later connections idle open so the chain cannot re-notify.
      return Promise.resolve(
        new Response(new ReadableStream<Uint8Array>({ start() {} }), {
          status: 200,
          headers: { "content-type": "text/event-stream" },
        }),
      );
    }) as typeof fetch;
    globalThis.document = doc as unknown as typeof globalThis.document;
    try {
      const backend = new OnlineBackend(
        {} as unknown as ManageRestClient,
        "t-a",
        new ProjectionClient("http://lesche.test"),
      );
      let nudges = 0;
      const first = backend.projectionFeed!.subscribe(() => {
        nudges += 1;
      });
      await new Promise((r) => setTimeout(r, 40));
      first(); // full detach takes the document listener along
      assertEquals(
        listeners.size,
        0,
        "the detach really removed the visibility listener",
      );
      const second = backend.projectionFeed!.subscribe(() => {
        nudges += 1;
      });
      // Revival stream, frame one (visible): leading nudge.
      await new Promise((r) => setTimeout(r, 60));
      assertEquals(nudges, 1, "the revived feed notifies while visible");
      doc.hidden = true;
      // Frame two (hidden): pends, no notify.
      await new Promise((r) => setTimeout(r, 120));
      assertEquals(nudges, 1, "hidden swallows the frame");
      doc.hidden = false;
      dispatch();
      await new Promise((r) => setTimeout(r, 10));
      assertEquals(
        nudges,
        2,
        "the flip after revival flushes the pending nudge",
      );
      second();
    } finally {
      globalThis.fetch = real;
      globalThis.document = realDoc;
    }
  },
);
