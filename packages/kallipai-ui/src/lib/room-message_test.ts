import { assert, assertEquals, assertFalse, assertThrows } from "@std/assert";
import {
  appendRoomLine,
  checkedMentions,
  decodeRoomMessage,
  encodeRoomSendMessage,
  MENTIONS_MAX,
  mentionChipsOf,
  parseParticipantHandle,
  roomLineFrom,
  roomLinePending,
  roomMessageLabel,
  roomRowAnchorId,
  roomRowAnnouncement,
  shortIdOf,
  type MentionTarget,
  type RoomLine,
} from "./room-message.ts";

/** A canonical participant id both codecs accept; the mention tests reuse it.
 *  Every case that needs a distinct id mints its own with crypto.randomUUID. */
const MENTION_ID = "6f9619ff-8b86-d011-b42d-00cf4fc964ff";

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
  // separator (archeion usernames are `[a-z0-9-]`, never `@`, but this locks the
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

Deno.test("decodeRoomMessage reads attachment:null as no attachment", () => {
  // The Rust side's serde Option maps a JSON null to None, so a null
  // attachment is a valid no-attachment frame -- it must render as
  // plain text, not leak null into consumers that read record_id.
  const bytes = new TextEncoder().encode('{"text":"hi","attachment":null}');
  const out = decodeRoomMessage(bytes);
  assertEquals(out, { op: "message", text: "hi" });
});

Deno.test("encodeRoomSendMessage attaches the file when given", () => {
  // With an attachment the JSON carries the structured descriptor; without
  // one the historical { text } shape is preserved byte-for-byte.
  const att = {
    record_id: "00000000-0000-0000-0000-000000000000",
    name: "report.pdf",
    size: 1234,
  };
  assertEquals(
    encodeRoomSendMessage("see attachment", { attachment: att }),
    '{"text":"see attachment","attachment":{"record_id":"00000000-0000-0000-0000-000000000000","name":"report.pdf","size":1234}}',
  );
  assertEquals(encodeRoomSendMessage("plain"), '{"text":"plain"}');
});

Deno.test("decodeRoomMessage reads an attachment payload", () => {
  const bytes = new TextEncoder().encode(
    '{"text":"see attachment","attachment":{"record_id":"abc","name":"a.zip","size":7}}',
  );
  assertEquals(decodeRoomMessage(bytes), {
    op: "message",
    text: "see attachment",
    attachment: { record_id: "abc", name: "a.zip", size: 7 },
  });
});

Deno.test("decodeRoomMessage warn-drops a malformed attachment", () => {
  // A malformed attachment drops the whole frame (same semantics as the
  // Rust side, where a serde failure loses the frame) rather than
  // rendering a half-valid file card.
  const bytes = new TextEncoder().encode(
    '{"text":"x","attachment":{"record_id":42}}',
  );
  assertEquals(decodeRoomMessage(bytes).op, "unknown");
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

Deno.test("roomLinePending is true for an own unsynced optimistic line", () => {
  const mine: Pick<RoomLine, "mine" | "seq"> = { mine: true, seq: -3 };
  assertEquals(roomLinePending(mine), true);
});

Deno.test(
  "roomLinePending is false once the own line holds its real seq",
  () => {
    const mine: Pick<RoomLine, "mine" | "seq"> = { mine: true, seq: 41 };
    assertEquals(roomLinePending(mine), false);
  },
);

Deno.test("roomLinePending is false for a live inbound line", () => {
  // The regression: a live inbound frame carries a synthetic NEGATIVE seq
  // (the SSE envelope has no server seq). The old `seq < 0` view check read
  // that as still-sending and left every received room message permanently
  // grey. mine is false for an inbound line, so it must never render pending.
  const inbound: Pick<RoomLine, "mine" | "seq"> = { mine: false, seq: -2 };
  assertEquals(roomLinePending(inbound), false);
  const inboundSynced: Pick<RoomLine, "mine" | "seq"> = {
    mine: false,
    seq: 42,
  };
  assertEquals(roomLinePending(inboundSynced), false);
});

// --- quoting: a room message names the row it answers by that row's seq ---

Deno.test("decodeRoomMessage reads a positive quote and drops the rest", () => {
  // Only a positive integer names a row: the Rust side reads `<= 0` as no
  // quote (there is no row 0), and a malformed field must not cost the frame.
  const decode = (raw: string) =>
    decodeRoomMessage(new TextEncoder().encode(raw));
  assertEquals(decode('{"text":"hi","quote":7}'), {
    op: "message",
    text: "hi",
    quote: 7,
  });
  for (const quote of ["0", "-4", "1.5", '"7"', "null", "{}"]) {
    assertEquals(decode(`{"text":"hi","quote":${quote}}`), {
      op: "message",
      text: "hi",
    });
  }
});

Deno.test("a quoted message keeps its attachment and its quote", () => {
  const attachment = { record_id: "r1", name: "a.pdf", size: 12 };
  const decoded = decodeRoomMessage(
    new TextEncoder().encode(
      JSON.stringify({ text: "", attachment, quote: 3 }),
    ),
  );
  assertEquals(decoded, { op: "message", text: "", attachment, quote: 3 });
});

Deno.test("a send still emits no quote field", () => {
  // The room composer cannot quote yet: the wire shape stays byte-identical to
  // what every tagma already accepts, so an older reader sees nothing new.
  assertEquals(encodeRoomSendMessage("hi"), '{"text":"hi"}');
  assertEquals(
    encodeRoomSendMessage("hi", {
      attachment: { record_id: "r1", name: "a.pdf", size: 12 },
    }),
    '{"text":"hi","attachment":{"record_id":"r1","name":"a.pdf","size":12}}',
  );
});

Deno.test("a decoded quote lands on the line", () => {
  const line = roomLineFrom(
    { op: "message", text: "reply", quote: 4 },
    { id: "u1", kind: "human", handle: "@ada" },
    { seq: 9, createdAt: "2026-10-21T00:00:00Z", selfId: "u2" },
  );
  assertEquals(line.quote, 4);
  assertEquals(line.text, "reply");
  assertEquals(line.senderHandle, "@ada");
  assertEquals(line.mine, false);
});

Deno.test("an unquoted line carries no quote key", () => {
  const line = roomLineFrom(
    { op: "message", text: "plain" },
    { id: "u1", kind: "human", handle: "@ada" },
    { seq: 9, createdAt: "2026-10-21T00:00:00Z", selfId: "u1" },
  );
  assertEquals("quote" in line, false);
  assertEquals(line.mine, true);
});

Deno.test("a wire quote reaches the rendered line", () => {
  // Decoding a field is not the same as the line carrying it: this is the seam
  // both inbound paths cross.
  const decoded = decodeRoomMessage(
    new TextEncoder().encode('{"text":"reply","quote":4}'),
  );
  if (decoded.op !== "message") throw new Error("expected a message frame");
  const line = roomLineFrom(
    decoded,
    { id: "u1", kind: "human", handle: "@ada" },
    { seq: 9, createdAt: "2026-10-21T00:00:00Z", selfId: "u1" },
  );
  assertEquals(line.quote, 4);
});

Deno.test("a wire mention list survives the decode", () => {
  // Wire-level, like the quote seam above: the ids a sender named must survive
  // the decode.
  const named = "6f9619ff-8b86-d011-b42d-00cf4fc964ff";
  const decoded = decodeRoomMessage(
    new TextEncoder().encode(
      JSON.stringify({ text: "look", mentions: [named] }),
    ),
  );
  assertEquals(decoded, { op: "message", text: "look", mentions: [named] });
});

Deno.test("a send carries mentions only when there are any", () => {
  assertEquals(encodeRoomSendMessage("hi"), '{"text":"hi"}');
  assertEquals(
    encodeRoomSendMessage("hi", { mentions: [MENTION_ID] }),
    '{"text":"hi","mentions":["' + MENTION_ID + '"]}',
  );
  // Either case names the same member, so what ships is the canonical
  // spelling of the id the picker chose.
  assertEquals(
    encodeRoomSendMessage("hi", { mentions: [MENTION_ID.toUpperCase()] }),
    '{"text":"hi","mentions":["' + MENTION_ID + '"]}',
  );
  // An empty list leaves the historical shape alone.
  assertEquals(encodeRoomSendMessage("hi", { mentions: [] }), '{"text":"hi"}');
});

Deno.test("a send refuses a mention it cannot honor", () => {
  // A picked member must never vanish into a send that looks like a mention and
  // names nobody, so an unusable list throws at the encoder.
  assertThrows(
    () => encodeRoomSendMessage("hi", { mentions: ["not-an-id"] }),
    Error,
    "not a participant id",
  );
  const many = Array.from({ length: MENTIONS_MAX + 1 }, () =>
    crypto.randomUUID(),
  );
  assertThrows(
    () => encodeRoomSendMessage("hi", { mentions: many }),
    Error,
    "more than",
  );
});

Deno.test("a send folds a member named twice into one", () => {
  // A repeat names the member already placed, so it costs nobody: the send goes
  // out with that member once, spelled the way the wire writes it. The
  // mixed-case pair pins the fold happening before the repeat is judged.
  const once = '{"text":"hi","mentions":["' + MENTION_ID + '"]}';
  assertEquals(
    encodeRoomSendMessage("hi", { mentions: [MENTION_ID, MENTION_ID] }),
    once,
  );
  assertEquals(
    encodeRoomSendMessage("hi", {
      mentions: [MENTION_ID.toUpperCase(), MENTION_ID],
    }),
    once,
  );
});

Deno.test("a send may carry the full list the cap allows", () => {
  // Exactly MENTIONS_MAX members is a send the encoder takes. The assertion
  // reads the encoder's own output: a decode roundtrip has a cap of its own, so
  // a widened encode would still trim on the way back and pass.
  const full = Array.from({ length: MENTIONS_MAX }, () => crypto.randomUUID());
  const sent: { mentions?: string[] } = JSON.parse(
    encodeRoomSendMessage("hi", { mentions: full }),
  );
  assertEquals(sent.mentions?.length, MENTIONS_MAX);
});

Deno.test("a repeat does not cost a full list its send", () => {
  // The cap counts members, not the entries the picker emitted: seventeen
  // entries naming sixteen members is a send of sixteen, so folding has to
  // happen before the cap is judged. A guard on the raw length fails here.
  const members = Array.from({ length: MENTIONS_MAX }, () =>
    crypto.randomUUID(),
  );
  const decoded = decodeRoomMessage(
    new TextEncoder().encode(
      encodeRoomSendMessage("hi", { mentions: [...members, members[3]] }),
    ),
  );
  if (decoded.op !== "message") throw new Error("expected a message frame");
  assertEquals(decoded.mentions, members);
});

Deno.test("a malformed mention entry costs only that entry", () => {
  const decoded = decodeRoomMessage(
    new TextEncoder().encode(
      JSON.stringify({
        text: "look",
        mentions: ["not-an-id", MENTION_ID, 42, null, { id: MENTION_ID }],
      }),
    ),
  );
  assertEquals(decoded, {
    op: "message",
    text: "look",
    mentions: [MENTION_ID],
  });
});

Deno.test("a non-list mention value costs no text", () => {
  const decoded = decodeRoomMessage(
    new TextEncoder().encode(JSON.stringify({ text: "look", mentions: "ada" })),
  );
  assertEquals(decoded, { op: "message", text: "look" });
});

Deno.test("mentions are deduplicated in the sender's order and capped", () => {
  const ids = Array.from({ length: MENTIONS_MAX + 4 }, () =>
    crypto.randomUUID(),
  );
  const decoded = decodeRoomMessage(
    new TextEncoder().encode(
      JSON.stringify({
        text: "look",
        mentions: [ids[1], ids[0], ids[1], ...ids],
      }),
    ),
  );
  if (decoded.op !== "message") throw new Error("expected a message frame");
  assertEquals(decoded.mentions?.length, MENTIONS_MAX);
  assertEquals(decoded.mentions?.slice(0, 2), [ids[1], ids[0]]);
});

Deno.test("the mention cap matches the wire contract", () => {
  // The normative number lives in the Rust crate, which cannot be imported
  // here, so this reads it: one source, and the constant below follows it. A
  // literal pin would let both sides move together and still disagree.
  const source = Deno.readTextFileSync(
    new URL(
      "../../../../crates/platform/kallipai-lesche-common/src/message.rs",
      import.meta.url,
    ),
  );
  const declared = /^pub const MENTIONS_MAX: usize = (\d+);$/m.exec(source);
  if (declared === null) {
    throw new Error("MENTIONS_MAX is not declared as expected");
  }
  assertEquals(MENTIONS_MAX, Number(declared[1]));
});

Deno.test("the mention refusals read the same on both sides", () => {
  // The two refusals live in the Rust crate as one-hole templates; this codec
  // fills the same holes. Pinning the template (not the filled sentence) puts
  // the anchor on the constant name and the hole shape, so a rename or a
  // reworded hole fails loudly here instead of drifting quietly on one side.
  const source = Deno.readTextFileSync(
    new URL(
      "../../../../crates/platform/kallipai-lesche-common/src/message.rs",
      import.meta.url,
    ),
  );
  const template = (name: string): string => {
    const hits = [
      ...source.matchAll(
        new RegExp(`^(?:pub )?const ${name}: &str = "(.*)";$`, "gm"),
      ),
    ];
    if (hits.length !== 1) {
      throw new Error(
        `${name} must be declared exactly once, saw ${hits.length}`,
      );
    }
    return hits[0][1];
  };
  const fill = (hole: string, value: string): string =>
    hole.replace("{}", value);
  const refusalText = (fn: () => unknown): string => {
    const caught = assertThrows(fn);
    return caught instanceof Error ? caught.message : String(caught);
  };

  assertEquals(
    refusalText(() => checkedMentions(["not-an-id"])),
    fill(template("MENTION_MALFORMED"), "not-an-id"),
  );
  const over = Array.from({ length: MENTIONS_MAX + 1 }, () =>
    crypto.randomUUID(),
  );
  assertEquals(
    refusalText(() => checkedMentions(over)),
    fill(template("MENTION_TOO_MANY"), String(MENTIONS_MAX)),
  );
});

Deno.test("only a hyphenated id names a participant", () => {
  // The same six spellings the Rust side holds: one shape in either case is
  // accepted, four other ways of writing that id are dropped.
  const canonical = crypto.randomUUID();
  const decoded = decodeRoomMessage(
    new TextEncoder().encode(
      JSON.stringify({
        text: "look",
        mentions: [
          canonical,
          canonical.toUpperCase(),
          `{${canonical}}`,
          canonical.replaceAll("-", ""),
          `urn:uuid:${canonical}`,
          "not-an-id",
        ],
      }),
    ),
  );
  if (decoded.op !== "message") throw new Error("expected a message frame");
  assertEquals(decoded.mentions, [canonical]);
});

Deno.test("a mention is stored in its canonical case", () => {
  // A writer that spells an id in upper case names the same member, so the
  // stored id must be the one form the comparison downstream relies on.
  const canonical = crypto.randomUUID();
  const decoded = decodeRoomMessage(
    new TextEncoder().encode(
      JSON.stringify({ text: "look", mentions: [canonical.toUpperCase()] }),
    ),
  );
  if (decoded.op !== "message") throw new Error("expected a message frame");
  assertEquals(decoded.mentions, [canonical]);
});

Deno.test("one member named in two cases counts once", () => {
  const canonical = crypto.randomUUID();
  const decoded = decodeRoomMessage(
    new TextEncoder().encode(
      JSON.stringify({
        text: "look",
        mentions: [canonical.toUpperCase(), canonical],
      }),
    ),
  );
  if (decoded.op !== "message") throw new Error("expected a message frame");
  assertEquals(decoded.mentions, [canonical]);
});

Deno.test("mentions reach the rendered line", () => {
  const decoded = decodeRoomMessage(
    new TextEncoder().encode(
      JSON.stringify({ text: "look", mentions: [MENTION_ID] }),
    ),
  );
  if (decoded.op !== "message") throw new Error("expected a message frame");
  const line = roomLineFrom(
    decoded,
    { id: "u1", kind: "human", handle: "@ada" },
    {
      seq: 9,
      createdAt: "2026-10-21T00:00:00Z",
      selfId: "u2",
    },
  );
  assertEquals(line.mentions, [MENTION_ID]);
});

// --- mention chips -------------------------------------------------------
// A chip is the view's projection of one mentioned id: the same handle
// vocabulary the message header draws, plus the two states the header never
// meets -- an id the roster cannot name, and the viewer's own mention.

const MENTION_TARGETS: readonly MentionTarget[] = [
  { id: MENTION_ID, kind: "human", handle: "@alice", label: "Alice" },
  {
    id: "11111111-2222-4333-8444-555555555555",
    kind: "agent",
    handle: "abc123@alice",
    label: "Scout",
    tagma_id: "8f6e0a4c-0000-4000-8000-000000000001",
  },
  {
    id: "22222222-3333-4444-8555-666666666666",
    kind: "agent",
    handle: "def456@alice",
  },
  {
    id: "33333333-4444-4555-8666-777777777777",
    kind: "human",
    handle: "user 333333",
  },
];

function rosterOf(
  targets: readonly MentionTarget[],
): Map<string, MentionTarget> {
  return new Map(targets.map((t) => [t.id, t] as const));
}

Deno.test("no mentions draw no chips", () => {
  assertEquals(
    mentionChipsOf(undefined, rosterOf(MENTION_TARGETS), null, true),
    [],
  );
  assertEquals(mentionChipsOf([], rosterOf(MENTION_TARGETS), null, true), []);
});

Deno.test(
  "a roster member resolves to its label, handle and profile link",
  () => {
    const [chip] = mentionChipsOf(
      [MENTION_ID],
      rosterOf(MENTION_TARGETS),
      null,
      true,
    );
    assertEquals(chip, {
      id: MENTION_ID,
      self: false,
      member: {
        kind: "human",
        handle: "@alice",
        label: "Alice",
        href: "/user/alice",
      },
    });
  },
);

Deno.test("an agent chip deep-links through its tagma id", () => {
  const [chip] = mentionChipsOf(
    ["11111111-2222-4333-8444-555555555555"],
    rosterOf(MENTION_TARGETS),
    null,
    true,
  );
  assertEquals(
    chip.member?.href,
    "/tagma/8f6e0a4c-0000-4000-8000-000000000001",
  );
});

Deno.test("an agent chip without a tagma id carries no link", () => {
  const [chip] = mentionChipsOf(
    ["22222222-3333-4444-8555-666666666666"],
    rosterOf(MENTION_TARGETS),
    null,
    true,
  );
  assertEquals(chip.member?.handle, "def456@alice");
  assertEquals(chip.member?.href, undefined);
});

Deno.test(
  "a roster member the registry could not resolve keeps that handle",
  () => {
    const [chip] = mentionChipsOf(
      ["33333333-4444-4555-8666-777777777777"],
      rosterOf(MENTION_TARGETS),
      null,
      true,
    );
    // The degraded `<kind> <prefix>` text renders verbatim, and it must not
    // become a link: the human arm's handle carries no `@`, and the agent arm
    // has no `tagma_id` to link through.
    assertEquals(chip.member, { kind: "human", handle: "user 333333" });
  },
);

Deno.test(
  "an id the landed roster does not hold is unknown, with no member block",
  () => {
    const id = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
    const [chip] = mentionChipsOf([id], new Map(), null, true);
    assertEquals(chip, { id, self: false, unnamed: "unknown" });
    // The wire carries ids alone, so an unknown member's kind is unknowable:
    // no member block (and so no kind word, no link) may be invented. The chip
    // still has the id, which is what the short-id token is cut from.
    assertEquals(chip.member, undefined);
    assertEquals(shortIdOf(chip.id), "aaaaaa");
  },
);

Deno.test("before the roster lands a miss reads unloaded, not unknown", () => {
  const id = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
  const [chip] = mentionChipsOf([id], new Map(), null, false);
  assertEquals(chip, { id, self: false, unnamed: "unloaded" });
  assertEquals(chip.member, undefined);
  // The flag only decides how a MISS reads: a hit is still named, so a roster
  // that arrives in pieces never un-names a member it already resolved.
  const [named] = mentionChipsOf(
    [MENTION_ID],
    rosterOf(MENTION_TARGETS),
    null,
    false,
  );
  assertEquals(named.member?.handle, "@alice");
});

Deno.test("the viewer's own mention is flagged in all three states", () => {
  const [named] = mentionChipsOf(
    [MENTION_ID],
    rosterOf(MENTION_TARGETS),
    MENTION_ID,
    true,
  );
  const ownId = "bbbbbbbb-cccc-4ddd-8eee-ffffffffffff";
  const [unknown] = mentionChipsOf([ownId], new Map(), ownId, true);
  const [unloaded] = mentionChipsOf([ownId], new Map(), ownId, false);
  assertEquals([named.self, unknown.self, unloaded.self], [true, true, true]);
  assertEquals(unknown.unnamed, "unknown");
  assertEquals(unloaded.unnamed, "unloaded");
});

Deno.test("chips keep the sender's order", () => {
  const ids = ["22222222-3333-4444-8555-666666666666", MENTION_ID];
  assertEquals(
    mentionChipsOf(ids, rosterOf(MENTION_TARGETS), null, true).map((c) => c.id),
    ids,
  );
});

Deno.test("a repeated id folds to its first chip", () => {
  // So the returned list is unique by construction and the component may key on
  // the id; case folding stays the decoder's job, which is why the repeat here
  // is an exact one.
  const ids = [MENTION_ID, MENTION_ID, "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"];
  assertEquals(
    mentionChipsOf(ids, rosterOf(MENTION_TARGETS), null, true).map((c) => c.id),
    [MENTION_ID, "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"],
  );
});

Deno.test("the short id is six code points, not six bytes", () => {
  assertEquals(shortIdOf("6f9619ff-8b86"), "6f9619");
  assertEquals(shortIdOf("abc"), "abc");
  assertEquals(shortIdOf("日本語のメンバー"), "日本語のメン");
});

Deno.test("a send carries the quoted seq only when it quotes one", () => {
  // Wire-level: this helper serializes the payload and the Rust `RoomMessage`
  // reads `quote` out of it, so the exact string pins the emitted shape, not
  // just that a key exists.
  assertEquals(
    encodeRoomSendMessage("hi", { quote: 7 }),
    '{"text":"hi","quote":7}',
  );
  assertEquals(
    encodeRoomSendMessage("hi", { quote: 7, mentions: [MENTION_ID] }),
    '{"text":"hi","quote":7,"mentions":["' + MENTION_ID + '"]}',
  );
});

Deno.test("a quote that cannot name a row fails the send", () => {
  // Same strictness as checkedMentions: a quote the wire cannot place is a
  // message that answers nothing, so it must not ship silently empty.
  for (const bad of [0, -3, 1.5, Number.NaN]) {
    assertThrows(() => encodeRoomSendMessage("hi", { quote: bad }));
  }
});

Deno.test("a quoted send reads back as the same seq", () => {
  const decoded = decodeRoomMessage(
    new TextEncoder().encode(encodeRoomSendMessage("hi", { quote: 12 })),
  );
  assertEquals(decoded, { op: "message", text: "hi", quote: 12 });
});

/** A row carrying the fields a spoken label reads. A case overrides one field
 *  at a time, so a failure names the input the label ignored. */
function labelled(over: Partial<RoomLine> = {}): RoomLine {
  return {
    seq: 7,
    senderId: MENTION_ID,
    senderKind: "human",
    senderHandle: "@alice",
    text: "hi",
    createdAt: "2026-10-23T10:00:00Z",
    mine: false,
    ...over,
  };
}

// A room row is a bare node in a log region: who wrote it, when, and whether it
// answers another message are spoken from this label alone. The cases below read
// the data (handle, time) and the relationships (quoting changes the sentence,
// the anchor matches the target row), never the catalog's wording, so a
// translation cannot fail them.

Deno.test("a room row's label names its author and its time", () => {
  const label = roomMessageLabel(labelled(), { time: "10:00 AM" });
  assert(label.trim().length > 0, "the row has to say something");
  assert(label.includes("@alice"), "the author belongs in the row's name");
  assert(label.includes("10:00 AM"), "so does when it was said");
});

Deno.test("a quoting row's label differs from a plain row's", () => {
  const plain = roomMessageLabel(labelled(), { time: "10:00 AM" });
  const quoting = roomMessageLabel(labelled({ quote: 3 }), {
    time: "10:00 AM",
  });
  assert(quoting !== plain, "answering a message changes what is spoken");
  assert(quoting.includes("@alice"), "and still names the author");
});

Deno.test(
  "the roster's name wins, then the handle, then the sender kind",
  () => {
    const named = roomMessageLabel(labelled(), {
      time: "10:00 AM",
      authorLabel: "Alice A.",
    });
    assert(named.includes("Alice A."), "a roster label replaces the handle");
    const handled = roomMessageLabel(labelled(), { time: "10:00 AM" });
    assert(handled.includes("@alice"), "with no roster the handle speaks");
    const anonymous = labelled({ senderHandle: "" });
    const human = roomMessageLabel(anonymous, { time: "10:00 AM" });
    const agent = roomMessageLabel(
      { ...anonymous, senderKind: "agent" },
      { time: "10:00 AM" },
    );
    assert(
      human.trim().length > 0 && agent.trim().length > 0,
      "never an empty name",
    );
    assert(agent !== human, "an agent row and a human row do not read alike");
  },
);

Deno.test("a jump to a room row names the author its label names", () => {
  const line = labelled();
  const spoken = roomRowAnnouncement(line, { time: "10:00 AM" });
  const label = roomMessageLabel(line, { time: "10:00 AM" });
  assert(spoken.includes("@alice"), "the jump names the author");
  assert(label.includes("@alice"), "and so does the row's own label");
  const named = roomRowAnnouncement(line, {
    time: "10:00 AM",
    authorLabel: "Alice A.",
  });
  assert(named.includes("Alice A."), "the roster name reaches the jump");
});

Deno.test("a jump to a room row says when it was sent", () => {
  const spoken = roomRowAnnouncement(labelled(), { time: "10:00 AM" });
  assert(spoken.includes("10:00 AM"), "the time belongs in the sentence");
  assert(spoken.includes("@alice"), "so does the author");
});

Deno.test("a room row with no time is named by its author alone", () => {
  const timed = roomRowAnnouncement(labelled(), { time: "10:00 AM" });
  const untimed = roomRowAnnouncement(labelled(), {});
  assert(untimed.includes("@alice"), "the author still speaks");
  assert(untimed !== timed, "a missing time drops the time, not the row");
  assertFalse(untimed.includes("10:00 AM"), "no dangling time");
});

Deno.test("a room row's anchor is its own name, not a shared one", () => {
  assertFalse(
    roomRowAnchorId(7).startsWith("msg-a11y-"),
    "room anchors do not shadow the bilateral ids",
  );
  assert(
    roomRowAnchorId(7) !== roomRowAnchorId(8),
    "two rows never share an anchor",
  );
});
