// RelayChannel end-to-end round-trip against a TS mock that plays the tagma
// relay's role (KEX + AEAD) using the same crypto.ts. Validates the full
// transport (openRelayChannel -> send -> encrypt -> lesche -> responder-decrypt
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
import { openRelayChannel, type RelayChannel } from "./channel.ts";
import { LescheApiError } from "./types.ts";
import type { Participant } from "./types.ts";
import { decodeB64, encodeB64 } from "@kallipai/kallip-common";
import type { LescheClient } from "./http.ts";
import type {
  Envelope,
  KeyExchangeInit,
  KeyExchangeResponse,
  TagmaControl,
  TagmaReply,
  TagmaRequest,
} from "./types.ts";

const enc = new TextEncoder();

/** A minimal tagma-relay mock for the data plane: the conversation + KEX +
 * envelope relay are served by the `lesche` mock; the pinned device key (TOFU
 * from the agora in production) is returned as `pinnedKeyB64` for the caller to
 * pass into `openRelayChannel`. On each posted envelope the lesche mock
 * decrypts the initiator's request and enqueues an `assistant_content` reply
 * back into the channel. */
function makeMock(deviceSecret: Uint8Array, tagmaId: string, convId: string) {
  const devicePub = ed25519.getPublicKey(deviceSecret);
  const pinnedKeyB64 = encodeB64(devicePub);
  let responderKey: Uint8Array | null = null;
  let channel: RelayChannel | null = null;
  let responderSeq = 0;
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
      conversation_id: convId,
      sender: { id: tagmaId, kind: "agent", handle: "Tagma" },
      sequence_n: seq,
      trace_id: "trace",
      timestamp: new Date().toISOString(),
      ciphertext: encodeB64(ct),
    });
  }

  const lesche = {
    createConversation(_t: string) {
      return { conversation_id: convId };
    },
    keyExchangeInit(_c: string, init: KeyExchangeInit): KeyExchangeResponse {
      const initiatorEph = decodeB64(init.ephemeral_public);
      const rpriv = x25519.utils.randomSecretKey();
      const rpub = x25519.getPublicKey(rpriv);
      const shared = x25519.scalarMult(rpriv, initiatorEph);
      responderKey = hkdf(sha256, shared, new Uint8Array(), HKDF_INFO, 32);
      const sig = ed25519.sign(
        kexTranscript(tagmaId, convId, initiatorEph, rpub),
        deviceSecret,
      );
      return { ephemeral_public: encodeB64(rpub), signature: encodeB64(sig) };
    },
    postEnvelope(_c: string, envelope: Envelope): void {
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
    },
  };
  return {
    lesche,
    pinnedKeyB64,
    setChannel: (c: RelayChannel) => {
      channel = c;
    },
    /** Push a raw responder reply (for dedup tests). */
    respond,
    receivedSeqs,
    lastRequest: () => lastRequest,
  };
}

Deno.test(
  "openRelayChannel + send round-trips against a mock tagma relay",
  async () => {
    const tagmaId = "tagma-1";
    const convId = "conv-1";
    const userId = "user-1";
    const deviceSecret = ed25519.utils.randomSecretKey();
    const { lesche, pinnedKeyB64, setChannel } = makeMock(
      deviceSecret,
      tagmaId,
      convId,
    );

    const channel = await openRelayChannel(
      lesche as unknown as LescheClient,
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
  },
);

Deno.test(
  "send increments sequence_n from 0 (the AEAD nonce counter)",
  async () => {
    const { lesche, pinnedKeyB64, setChannel, receivedSeqs } = makeMock(
      ed25519.utils.randomSecretKey(),
      "tagma-s",
      "conv-s",
    );
    const channel = await openRelayChannel(
      lesche as unknown as LescheClient,
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
  },
);

Deno.test(
  "an undecryptable inbound envelope is dropped, not yielded",
  async () => {
    const { lesche, pinnedKeyB64, setChannel } = makeMock(
      ed25519.utils.randomSecretKey(),
      "tagma-d",
      "conv-d",
    );
    const channel = await openRelayChannel(
      lesche as unknown as LescheClient,
      "tagma-d",
      "u",
      "Alice",
      pinnedKeyB64,
    );
    setChannel(channel);
    // A garbage-ciphertext envelope (the SSE demux would route this in) that
    // cannot decrypt under the session key.
    channel.enqueue({
      conversation_id: channel.conversationId,
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
  },
);

Deno.test(
  "send surfaces a postEnvelope 503 (tagma offline) as a rejection",
  async () => {
    const { lesche, pinnedKeyB64, setChannel } = makeMock(
      ed25519.utils.randomSecretKey(),
      "tagma-503",
      "conv-503",
    );
    const channel = await openRelayChannel(
      lesche as unknown as LescheClient,
      "tagma-503",
      "u",
      "Alice",
      pinnedKeyB64,
    );
    setChannel(channel);
    // Swap postEnvelope to a 503 (the lesche returns 503 "tagma offline").
    const overridable = lesche as unknown as { postEnvelope: () => void };
    overridable.postEnvelope = () => {
      throw new LescheApiError(503, "tagma is offline");
    };
    let caught: unknown;
    try {
      await channel.send("x");
    } catch (e) {
      caught = e;
    }
    if (!(caught instanceof LescheApiError) || caught.status !== 503) {
      throw new Error(
        `expected LescheApiError(503), got ${JSON.stringify(caught)}`,
      );
    }
    channel.close();
  },
);

Deno.test(
  "replies yields every frame without dedup (dedup is the UI store's job)",
  async () => {
    // The channel is a pure transport: it does NOT dedup by history_id. Two
    // deliveries of the same frame both yield — ordering/dedup across batch
    // replay and live frames is the UI store's responsibility (it owns the
    // rendered cursor and the local cache).
    const { lesche, pinnedKeyB64, setChannel, respond } = makeMock(
      ed25519.utils.randomSecretKey(),
      "tagma-h",
      "conv-h",
    );
    const channel = await openRelayChannel(
      lesche as unknown as LescheClient,
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
  },
);

Deno.test("history() sends a cursor-based history control op", async () => {
  const { lesche, pinnedKeyB64, setChannel, lastRequest } = makeMock(
    ed25519.utils.randomSecretKey(),
    "tagma-c",
    "conv-c",
  );
  const channel = await openRelayChannel(
    lesche as unknown as LescheClient,
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
