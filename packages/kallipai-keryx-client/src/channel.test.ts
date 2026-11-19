// RelayChannel end-to-end round-trip against a TS mock that plays the tagma
// relay's role (KEX + AEAD) using the same crypto.ts. Validates the full
// transport (openRelayChannel -> send -> encrypt -> keryx -> responder-decrypt
// -> responder encrypt reply -> initiator decrypt -> TagmaReply) without the
// live backend.

import { assertEquals, assertExists } from "@std/assert";
import { ed25519, x25519 } from "@noble/curves/ed25519.js";
import { hkdf } from "@noble/hashes/hkdf.js";
import { sha256 } from "@noble/hashes/sha2.js";
import {
  aeadDecrypt,
  aeadEncrypt,
  DIR_INITIATOR_TO_RESPONDER,
  DIR_RESPONDER_TO_INITIATOR,
  HKDF_INFO,
  kexTranscript,
} from "./crypto.ts";
import {
  ManageChannelError,
  openRelayChannel,
  type RelayChannel,
} from "./channel.ts";
import { DataPlaneApiError } from "./types.ts";
import type { Participant } from "./types.ts";
import { decodeB64, encodeB64 } from "@kallipai/kallipai-common";
import { withFetch } from "@kallipai/kallipai-common/testing";
import type {
  Envelope,
  KeyExchangeInit,
  TagmaControl,
  TagmaReply,
  TagmaRequest,
} from "./types.ts";

const enc = new TextEncoder();

const BASE = "http://keryx.test";
interface Hop {
  readonly method: string;
  readonly path: string;
  readonly body: unknown;
}
const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status });
interface Relay {
  handle(hop: Hop): Response;
}
/** The relay the stub routes to; each `makeMock` replaces it. Before the
 * first `makeMock` nothing answers, so a stray dial fails instead of
 * reaching the network. */
const NOTHING: Relay = { handle: () => new Response(null, { status: 404 }) };
let relay: Relay = NOTHING;
const stub = ((input: string | URL, init?: RequestInit) => {
  const url = new URL(String(input));
  return Promise.resolve(
    relay.handle({
      method: init?.method ?? "GET",
      path: url.pathname,
      body: init?.body ? JSON.parse(String(init.body)) : null,
    }),
  );
}) as typeof fetch;
/** Every case runs against the stub. */
function test(name: string, run: () => Promise<void>): void {
  Deno.test(name, () => withFetch(stub, run));
}
/** A minimal tagma-relay mock for the data plane: the fetch stub answers the
 * conversation, KEX and envelope routes the verbs dial and plays the
 * responder's half of the AEAD exchange. The pinned device key (TOFU from the
 * archeion in production) is returned as `pinnedKeyB64` for the caller to pass
 * into `openRelayChannel`. On each posted envelope the mock decrypts the
 * initiator's request and enqueues an `assistant_content` reply back into the
 * channel. */
function makeMock(deviceSecret: Uint8Array, tagmaId: string, convId: string) {
  const devicePub = ed25519.getPublicKey(deviceSecret);
  const pinnedKeyB64 = encodeB64(devicePub);
  let responderKey: Uint8Array | null = null;
  let channel: RelayChannel | null = null;
  let responderSeq = 0;
  let envelopeFailure = 0;
  // Captured state for assertions: the initiator-originated sequence numbers
  // seen and the most-recent decrypted request/control.
  const receivedSeqs: number[] = [];
  let lastRequest: TagmaRequest | TagmaControl | null = null;

  /** Encrypt `reply` (dir=1, the responder's counter) and feed it back through
   * the channel's SSE-demux path. A no-op until `setChannel` wires the channel
   * (a real tagma's post-KEX replay would be dropped the same way if it landed
   * before the SSE demux is wired — the next reconnect re-replays). */
  function respond(reply: TagmaReply) {
    if (!channel) return;
    const seq = responderSeq++;
    const ct = aeadEncrypt(
      responderKey!,
      DIR_RESPONDER_TO_INITIATOR,
      seq,
      enc.encode(JSON.stringify(reply)),
    );
    channel.enqueue({
      channel_id: convId,
      sender: { id: tagmaId, kind: "agent", handle: "Tagma" },
      sequence_n: seq,
      trace_id: "trace",
      timestamp: new Date().toISOString(),
      ciphertext: encodeB64(ct),
    });
  }

  relay = {
    handle(hop: Hop): Response {
      if (hop.method === "POST" && hop.path === "/conversations") {
        return json({ conversation_id: convId });
      }
      if (
        hop.method === "POST" &&
        hop.path === `/conversations/${convId}/key-exchange/init`
      ) {
        const init = hop.body as KeyExchangeInit;
        const initiatorEph = decodeB64(init.ephemeral_public);
        const rpriv = x25519.utils.randomSecretKey();
        const rpub = x25519.getPublicKey(rpriv);
        const shared = x25519.scalarMult(rpriv, initiatorEph);
        responderKey = hkdf(sha256, shared, new Uint8Array(), HKDF_INFO, 32);
        const sig = ed25519.sign(
          kexTranscript(tagmaId, convId, initiatorEph, rpub),
          deviceSecret,
        );
        return json({
          ephemeral_public: encodeB64(rpub),
          signature: encodeB64(sig),
        });
      }
      if (
        hop.method === "POST" &&
        hop.path === `/conversations/${convId}/envelopes`
      ) {
        if (envelopeFailure !== 0) {
          return json(
            { error: { message: "tagma is offline" } },
            envelopeFailure,
          );
        }
        const envelope = hop.body as Envelope;
        // Responder decrypts the initiator's request (dir=0).
        const pt = aeadDecrypt(
          responderKey!,
          DIR_INITIATOR_TO_RESPONDER,
          envelope.sequence_n,
          decodeB64(envelope.ciphertext),
        );
        assertExists(pt, "responder must decrypt the initiator's envelope");
        const req = JSON.parse(new TextDecoder().decode(pt)) as
          | TagmaRequest
          | TagmaControl;
        lastRequest = req;
        receivedSeqs.push(envelope.sequence_n);
        // A history control op carries no agent action; the mock just records it
        // (a real tagma would reply with the batch). Otherwise echo the text.
        if (req.op === "send_message") {
          respond({
            kind: "event",
            event: { type: "assistant_content", content: `echo:${req.text}` },
            history_id: 0,
          });
        }
        return new Response(null, {
          status: 202,
          headers: { "content-length": "0" },
        });
      }
      return json({ error: { message: "no such route" } }, 404);
    },
  };
  return {
    base: BASE,
    pinnedKeyB64,
    setChannel: (c: RelayChannel) => {
      channel = c;
    },
    failEnvelopes: (status: number) => {
      envelopeFailure = status;
    },
    /** Push a raw responder reply (for dedup tests). */
    respond,
    receivedSeqs,
    lastRequest: () => lastRequest,
  };
}

test("openRelayChannel + send round-trips against a mock tagma relay", async () => {
  const tagmaId = "tagma-1";
  const convId = "conv-1";
  const userId = "user-1";
  const deviceSecret = ed25519.utils.randomSecretKey();
  const { base, pinnedKeyB64, setChannel } = makeMock(
    deviceSecret,
    tagmaId,
    convId,
  );

  const channel = await openRelayChannel(
    base,
    tagmaId,
    userId,
    "Alice",
    pinnedKeyB64,
  );
  assertEquals(channel.conversationId, convId);
  assertEquals(channel.tagmaId, tagmaId);
  setChannel(channel);

  await channel.send("hello");

  const iter = channel.replies();
  const first = await iter.next();
  assertEquals(first.done, false);
  const reply = (first.value as { sender: Participant; reply: TagmaReply })
    .reply;
  if (reply.kind !== "event") {
    throw new Error(`expected event, got ${reply.kind}`);
  }
  if (reply.event.type !== "assistant_content") {
    throw new Error(`expected assistant_content, got ${reply.event.type}`);
  }
  assertEquals(reply.event.content, "echo:hello");
  channel.close();
  assertEquals((await iter.next()).done, true);
});

test("send increments sequence_n from 0 (the AEAD nonce counter)", async () => {
  const { base, pinnedKeyB64, setChannel, receivedSeqs } = makeMock(
    ed25519.utils.randomSecretKey(),
    "tagma-s",
    "conv-s",
  );
  const channel = await openRelayChannel(
    base,
    "tagma-s",
    "u",
    "Alice",
    pinnedKeyB64,
  );
  setChannel(channel);
  await channel.send("a");
  await channel.send("b");
  assertEquals(receivedSeqs, [0, 1]);
  channel.close();
});

test("an undecryptable inbound envelope is dropped, not yielded", async () => {
  const { base, pinnedKeyB64, setChannel } = makeMock(
    ed25519.utils.randomSecretKey(),
    "tagma-d",
    "conv-d",
  );
  const channel = await openRelayChannel(
    base,
    "tagma-d",
    "u",
    "Alice",
    pinnedKeyB64,
  );
  setChannel(channel);
  // A garbage-ciphertext envelope (the SSE demux would route this in) that
  // cannot decrypt under the session key.
  channel.enqueue({
    channel_id: channel.conversationId,
    sender: { id: "tagma-d", kind: "agent", handle: "Tagma" },
    sequence_n: 99,
    trace_id: "t",
    timestamp: new Date().toISOString(),
    ciphertext: "AAAA",
  });
  // A valid send right after produces a real reply, which must be the FIRST
  // yielded value (the tampered one was dropped).
  await channel.send("ok");
  const iter = channel.replies();
  const first = await iter.next();
  const reply = (first.value as { sender: Participant; reply: TagmaReply })
    .reply;
  if (reply.kind !== "event" || reply.event.type !== "assistant_content") {
    throw new Error(
      `tampered envelope was not dropped; got ${JSON.stringify(reply)}`,
    );
  }
  channel.close();
});

test("send surfaces a postEnvelope 503 (tagma offline) as a rejection", async () => {
  const { base, pinnedKeyB64, setChannel, failEnvelopes } = makeMock(
    ed25519.utils.randomSecretKey(),
    "tagma-503",
    "conv-503",
  );
  const channel = await openRelayChannel(
    base,
    "tagma-503",
    "u",
    "Alice",
    pinnedKeyB64,
  );
  setChannel(channel);
  // The keryx answers 503 "tagma offline" on the envelope route.
  failEnvelopes(503);
  let caught: unknown;
  try {
    await channel.send("x");
  } catch (e) {
    caught = e;
  }
  if (!(caught instanceof DataPlaneApiError) || caught.status !== 503) {
    throw new Error(
      `expected DataPlaneApiError(503), got ${JSON.stringify(caught)}`,
    );
  }
  channel.close();
});

test("replies yields every frame without dedup (dedup is the UI store's job)", async () => {
  // The channel is a pure transport: it does NOT dedup by history_id. Two
  // deliveries of the same frame both yield — ordering/dedup across batch
  // replay and live frames is the UI store's responsibility (it owns the
  // rendered cursor and the local cache).
  const { base, pinnedKeyB64, setChannel, respond } = makeMock(
    ed25519.utils.randomSecretKey(),
    "tagma-h",
    "conv-h",
  );
  const channel = await openRelayChannel(
    base,
    "tagma-h",
    "u",
    "Alice",
    pinnedKeyB64,
  );
  setChannel(channel);

  const event = (id: number): TagmaReply => ({
    kind: "event",
    event: { type: "assistant_content", content: `m${id}` },
    history_id: id,
  });
  respond(event(1));
  respond(event(1)); // duplicate — yielded again; the store drops it
  respond(event(2));

  const iter = channel.replies();
  const a = ((await iter.next()).value as { reply: TagmaReply }).reply;
  const b = ((await iter.next()).value as { reply: TagmaReply }).reply;
  const c = ((await iter.next()).value as { reply: TagmaReply }).reply;
  assertEquals((a as { event: { content: string } }).event.content, "m1");
  assertEquals(
    (b as { event: { content: string } }).event.content,
    "m1",
    "the channel yields the duplicate; the store dedups",
  );
  assertEquals((c as { event: { content: string } }).event.content, "m2");
  channel.close();
});

test("history() sends a cursor-based history control op", async () => {
  const { base, pinnedKeyB64, setChannel, lastRequest } = makeMock(
    ed25519.utils.randomSecretKey(),
    "tagma-c",
    "conv-c",
  );
  const channel = await openRelayChannel(
    base,
    "tagma-c",
    "u",
    "Alice",
    pinnedKeyB64,
  );
  setChannel(channel);
  await channel.history({ after: 5, limit: 20 });
  const req = lastRequest();
  if (!req || req.op !== "history") {
    throw new Error(`expected history control op, got ${JSON.stringify(req)}`);
  }
  assertEquals(req.after, 5);
  assertEquals(req.before, null);
  assertEquals(req.limit, 20);
  channel.close();
});

test("manage() round-trips: request and response correlation", async () => {
  const { base, pinnedKeyB64, setChannel, respond, lastRequest } = makeMock(
    ed25519.utils.randomSecretKey(),
    "tagma-m",
    "conv-m",
  );
  const channel = await openRelayChannel(
    base,
    "tagma-m",
    "u",
    "Alice",
    pinnedKeyB64,
  );
  setChannel(channel);

  const managePromise = channel.manage("GET", "/budget");
  // The mock captured the manage control op (including req_id).
  const req = lastRequest() as TagmaControl | null;
  if (!req || req.op !== "manage") {
    throw new Error(`expected manage control, got ${JSON.stringify(req)}`);
  }
  assertEquals(req.method, "GET");
  assertEquals(req.path, "/budget");
  // Respond with a manage_result matching the req_id.
  respond({
    kind: "manage_result",
    req_id: req.req_id,
    status: 200,
    body: { total: 5000 },
  });
  const result = await managePromise;
  assertEquals(result.status, 200);
  assertEquals(result.body, { total: 5000 });
  channel.close();
});

test("close() rejects pending manage() promises", async () => {
  const { base, pinnedKeyB64, setChannel } = makeMock(
    ed25519.utils.randomSecretKey(),
    "tagma-c2",
    "conv-c2",
  );
  const channel = await openRelayChannel(
    base,
    "tagma-c2",
    "u",
    "Alice",
    pinnedKeyB64,
  );
  setChannel(channel);

  const managePromise = channel.manage("GET", "/agents");
  // Don't respond — close instead.
  channel.close();
  let caught: unknown;
  try {
    await managePromise;
  } catch (e) {
    caught = e;
  }
  if (!(caught instanceof ManageChannelError) || caught.reason !== "closed") {
    throw new Error(
      `expected a closed-channel rejection, got ${JSON.stringify(caught)}`,
    );
  }
  assertEquals(caught.message, "channel closed");
});

test("manage() honours a per-call timeoutMs", async () => {
  const { base, pinnedKeyB64, setChannel } = makeMock(
    ed25519.utils.randomSecretKey(),
    "tagma-t2",
    "conv-t2",
  );
  const channel = await openRelayChannel(
    base,
    "tagma-t2",
    "u",
    "Alice",
    pinnedKeyB64,
  );
  setChannel(channel);

  // Nobody answers: only the caller's deadline can end the wait, so the
  // 15s default would leave this test hanging.
  const started = Date.now();
  let caught: unknown;
  try {
    await channel.manage("POST", "/profiles/probe", null, {
      timeoutMs: 20,
    });
  } catch (e) {
    caught = e;
  }
  const elapsed = Date.now() - started;
  channel.close();
  if (!(caught instanceof ManageChannelError) || caught.reason !== "timeout") {
    throw new Error(
      `expected a manage timeout rejection, got ${JSON.stringify(caught)}`,
    );
  }
  assertEquals(caught.message, "manage timeout: POST /profiles/probe");
  assertEquals(
    elapsed < 5_000,
    true,
    "the per-call deadline ended the wait, not the 15s default",
  );
});

test("manage() on a closed channel rejects without waiting", async () => {
  const { base, pinnedKeyB64, setChannel } = makeMock(
    ed25519.utils.randomSecretKey(),
    "tagma-c3",
    "conv-c3",
  );
  const channel = await openRelayChannel(
    base,
    "tagma-c3",
    "u",
    "Alice",
    pinnedKeyB64,
  );
  setChannel(channel);
  channel.close();

  // A closed channel rejects at once: registering the request would park
  // it until the caller's deadline, which is 90s for the probe.
  const started = Date.now();
  const caught = await channel.manage("GET", "/agents").catch((e) => e);
  assertEquals(Date.now() - started < 1_000, true);
  assertEquals(caught instanceof ManageChannelError, true);
  assertEquals((caught as ManageChannelError).reason, "closed");
});

test("send carries the attachment on the send_message op", async () => {
  const { base, pinnedKeyB64, setChannel, lastRequest } = makeMock(
    ed25519.utils.randomSecretKey(),
    "tagma-att",
    "conv-att",
  );
  const channel = await openRelayChannel(
    base,
    "tagma-att",
    "u",
    "Alice",
    pinnedKeyB64,
  );
  setChannel(channel);

  // Attachment-only send: empty text, the file reference rides along.
  await channel.send("", { record_id: "rec-1", name: "notes.txt", size: 3 });

  const req = lastRequest();
  if (req?.op !== "send_message") {
    throw new Error(`expected send_message, got ${req?.op}`);
  }
  assertEquals(req.text, "");
  assertEquals(req.attachment, {
    record_id: "rec-1",
    name: "notes.txt",
    size: 3,
  });
  channel.close();
});
