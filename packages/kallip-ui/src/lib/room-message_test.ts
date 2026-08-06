import { assertEquals } from "@std/assert";
import {
  appendRoomLine,
  decodeRoomMessage,
  encodeRoomSendMessage,
  parseParticipantHandle,
  type RoomLine,
} from "./room-message.ts";

// The room-message plaintext is a JSON-serialized `RoomMessage { text }`
// (rooms and the bilateral 1:1 path are disjoint; a room message is just text).
// These tests pin the TS<->Rust wire contract: the emitted JSON must round-trip
// through the tagma's `serde_json::from_slice::<RoomMessage>`, and the decoder
// must read what a tagma (or another browser) sends.

Deno.test("parseParticipantHandle decomposes an agent handle", () => {
  // An agent handle is `<short-id>@<owner>`; split on the first `@` so the
  // owner handle is shown with a leading `@` and the short id becomes a
  // separate token.
  assertEquals(parseParticipantHandle("a4f2b9@alice", "agent"), {
    handle: "@alice",
    shortId: "a4f2b9",
  });
  // A hyphenated owner username must not over-split: only the FIRST `@` is the
  // separator (agora usernames are `[a-z0-9-]`, never `@`, but this locks the
  // split-once behavior regardless).
  assertEquals(parseParticipantHandle("a4f2b9@alice-doe", "agent"), {
    handle: "@alice-doe",
    shortId: "a4f2b9",
  });
});

Deno.test("parseParticipantHandle passes a human handle through", () => {
  // A resolved human handle is already `@<username>` -- no short id.
  assertEquals(parseParticipantHandle("@alice", "human"), { handle: "@alice" });
  // The degraded form (a registry miss at the relay, or the optimistic local
  // line) is `"user <prefix>"`; it is passed through VERBATIM, never fabricated
  // into an `@` (which would forge a username that cannot exist).
  assertEquals(parseParticipantHandle("user abc123", "human"), {
    handle: "user abc123",
  });
  // A bare value is likewise passed through verbatim -- the rule is "never
  // fabricate an `@`", locked against a future defensive regression.
  assertEquals(parseParticipantHandle("alice", "human"), { handle: "alice" });
});

Deno.test("parseParticipantHandle degrades on a malformed handle", () => {
  // An agent handle without an `@` (a bad frame) falls back to the raw string
  // with no short id, so render never crashes.
  assertEquals(parseParticipantHandle("garbage", "agent"), {
    handle: "garbage",
  });
});

Deno.test("encodeRoomSendMessage emits the { text } shape", () => {
  // Mirrors RoomMessage { text } as serde_json::to_string produces:
  // {"text":"..."}.
  assertEquals(encodeRoomSendMessage("hello"), '{"text":"hello"}');
});

Deno.test("decodeRoomMessage reads a { text } payload", () => {
  const bytes = new TextEncoder().encode('{"text":"hi"}');
  assertEquals(decodeRoomMessage(bytes), { op: "message", text: "hi" });
});

Deno.test(
  "decodeRoomMessage surfaces a payload without text as unknown",
  () => {
    // A shape without a string `text` (a future fielded variant, or a control
    // frame) is not chat text; the caller warn-drops it rather than mis-rendering.
    const bytes = new TextEncoder().encode('{"op":"interrupt","req_id":3}');
    const out = decodeRoomMessage(bytes);
    assertEquals(out.op, "unknown");
  },
);

Deno.test("decodeRoomMessage surfaces malformed JSON as unknown", () => {
  const bytes = new TextEncoder().encode("not json");
  const out = decodeRoomMessage(bytes);
  assertEquals(out.op, "unknown");
  assertEquals(out.op === "unknown" ? out.raw : "", "not json");
});

Deno.test("encode + decode round-trip a chat line", () => {
  const text = 'a multi-line\nmessage with "quotes" + üñıçødé';
  const decoded = decodeRoomMessage(
    new TextEncoder().encode(encodeRoomSendMessage(text)),
  );
  assertEquals(decoded, { op: "message", text });
});

// `appendRoomLine` is the store's optimistic-send dedup, extracted pure so it
// is unit-testable without the Svelte runtime (the reactive store itself is
// verified by svelte-check + the browser integration). The sender identity
// fields (`senderKind`/`senderHandle`) are carried through but irrelevant to
// dedup, which keys on `(seq, senderId, text)`; the fixtures set neutral
// values.

Deno.test("appendRoomLine appends a fresh line", () => {
  const a: RoomLine = {
    seq: 1,
    senderId: "alice",
    senderKind: "human",
    senderHandle: "Alice",
    text: "hi",
    createdAt: "t1",
    mine: false,
  };
  const b: RoomLine = {
    seq: 2,
    senderId: "bob",
    senderKind: "human",
    senderHandle: "Bob",
    text: "yo",
    createdAt: "t2",
    mine: false,
  };
  assertEquals(appendRoomLine([a], b), [a, b]);
});

Deno.test(
  "appendRoomLine replaces an optimistic send line when the real one lands",
  () => {
    // The optimistic line has a negative seq (synthetic); the echoed history row
    // carries the real seq. Same mine + text -> replace, not duplicate.
    const pending: RoomLine = {
      seq: -1,
      senderId: "me",
      senderKind: "human",
      senderHandle: "me",
      text: "hello",
      createdAt: "t0",
      mine: true,
    };
    const real: RoomLine = {
      seq: 7,
      senderId: "me",
      senderKind: "human",
      senderHandle: "me",
      text: "hello",
      createdAt: "t1",
      mine: true,
    };
    assertEquals(appendRoomLine([pending], real), [real]);
  },
);

Deno.test(
  "appendRoomLine does NOT dedup a not-mine echo against a pending mine line",
  () => {
    // A different sender's line with the same text must not collapse the pending
    // optimistic line (the send has not echoed back yet).
    const pending: RoomLine = {
      seq: -1,
      senderId: "me",
      senderKind: "human",
      senderHandle: "me",
      text: "hello",
      createdAt: "t0",
      mine: true,
    };
    const other: RoomLine = {
      seq: 7,
      senderId: "alice",
      senderKind: "human",
      senderHandle: "Alice",
      text: "hello",
      createdAt: "t1",
      mine: false,
    };
    assertEquals(appendRoomLine([pending], other), [pending, other]);
  },
);

Deno.test(
  "appendRoomLine does NOT dedup two distinct optimistic sends with the same text",
  () => {
    // Two pending mine lines with the same text (the user sent "ok" twice) are
    // distinct; only a CONFIRMED (positive seq) line collapses its pending twin.
    const first: RoomLine = {
      seq: -1,
      senderId: "me",
      senderKind: "human",
      senderHandle: "me",
      text: "ok",
      createdAt: "t0",
      mine: true,
    };
    const second: RoomLine = {
      seq: -2,
      senderId: "me",
      senderKind: "human",
      senderHandle: "me",
      text: "ok",
      createdAt: "t1",
      mine: true,
    };
    assertEquals(appendRoomLine([first], second), [first, second]);
  },
);

Deno.test(
  "appendRoomLine collapses a RECEIVED live frame when its history echo lands",
  () => {
    // A live frame from another member is appended with a synthetic negative
    // seq; when the history echo arrives at its real seq, the pending live line
    // is REPLACED (not duplicated) -- keyed on (sender, text), mine-agnostic.
    const pendingLive: RoomLine = {
      seq: -1709876543,
      senderId: "alice",
      senderKind: "agent",
      senderHandle: "a4f2b9@alice",
      text: "hi",
      createdAt: "t0",
      mine: false,
    };
    const echo: RoomLine = {
      seq: 9,
      senderId: "alice",
      senderKind: "agent",
      senderHandle: "a4f2b9@alice",
      text: "hi",
      createdAt: "t1",
      mine: false,
    };
    assertEquals(appendRoomLine([pendingLive], echo), [echo]);
  },
);

Deno.test(
  "appendRoomLine drops a live frame whose confirmed echo is already shown",
  () => {
    // Symmetric to the case above: the history fetch won the race over SSE, so
    // the row is already rendered at its real positive seq when the buffered
    // live frame (synthetic negative seq) arrives. The live frame is DROPPED
    // (no duplicate), keyed on (sender, text). Without this guard the live
    // frame bypasses the seq guard and would render a permanent duplicate.
    const confirmed: RoomLine = {
      seq: 9,
      senderId: "alice",
      senderKind: "agent",
      senderHandle: "Helper",
      text: "hi",
      createdAt: "t1",
      mine: false,
    };
    const lateLive: RoomLine = {
      seq: -1709876543,
      senderId: "alice",
      senderKind: "agent",
      senderHandle: "Helper",
      text: "hi",
      createdAt: "t0",
      mine: false,
    };
    assertEquals(appendRoomLine([confirmed], lateLive), [confirmed]);
  },
);

Deno.test(
  "appendRoomLine keeps a live frame with no confirmed echo (a different sender's same-text row)",
  () => {
    // The symmetric guard keys on senderId: a live frame from Bob is NOT dropped
    // just because Alice has a confirmed line with the same text.
    const aliceConfirmed: RoomLine = {
      seq: 9,
      senderId: "alice",
      senderKind: "human",
      senderHandle: "@alice",
      text: "hi",
      createdAt: "t1",
      mine: false,
    };
    const bobLive: RoomLine = {
      seq: -2,
      senderId: "bob",
      senderKind: "human",
      senderHandle: "@bob",
      text: "hi",
      createdAt: "t0",
      mine: false,
    };
    assertEquals(appendRoomLine([aliceConfirmed], bobLive), [
      aliceConfirmed,
      bobLive,
    ]);
  },
);
