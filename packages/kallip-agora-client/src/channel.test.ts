// RelayChannel end-to-end round-trip against a TS mock that plays the herald's
// role (KEX + AEAD) using the same crypto.ts. Validates the full transport
// (openRelayChannel -> send -> encrypt -> lesche -> herald-decrypt -> herald
// encrypt reply -> app decrypt -> TagmaReply) without the live backend. The
// cross-endpoint check against the real Rust herald happens in Phase 2.

import { assertExists, assertEquals } from "@std/assert";
import { ed25519, x25519 } from "@noble/curves/ed25519.js";
import { hkdf } from "@noble/hashes/hkdf.js";
import { sha256 } from "@noble/hashes/sha2.js";
import {
  aeadDecrypt,
  aeadEncrypt,
  DIR_APP_TO_HERALD,
  DIR_HERALD_TO_APP,
  HKDF_INFO,
  kexTranscript,
} from "./crypto.ts";
import { openRelayChannel, type RelayChannel } from "./channel.ts";
import { AgoraApiError } from "./types.ts";
import { decodeB64, encodeB64 } from "./base64.ts";
import type { AgoraClient, LescheClient } from "./http.ts";
import type {
  Envelope,
  KeyExchangeInit,
  KeyExchangeResponse,
  TagmaReply,
  TagmaRequest,
} from "./types.ts";

const enc = new TextEncoder();

/** A minimal herald-side mock split across the agora/lesche boundary: the
 * pinned key is served by the `agora` mock (getTagma), the conversation + KEX +
 * envelope relay by the `lesche` mock. On each posted envelope the lesche mock
 * decrypts the app's request and enqueues a `Finished` reply back into the
 * channel. */
function makeMock(deviceSecret: Uint8Array, tagmaId: string, convId: string) {
  const devicePub = ed25519.getPublicKey(deviceSecret);
  let heraldKey: Uint8Array | null = null;
  let channel: RelayChannel | null = null;
  let heraldSeq = 0;
  // Captured state for assertions: the app-originated sequence numbers seen and
  // the most-recent decrypted request.
  const receivedSeqs: number[] = [];
  let lastRequest: TagmaRequest | null = null;

  const agora = {
    getTagma(_id: string) {
      return { tagma_id: tagmaId, pinned_public_key: encodeB64(devicePub) };
    },
  };
  const lesche = {
    createConversation(_t: string) {
      return { conversation_id: convId };
    },
    keyExchangeInit(_c: string, init: KeyExchangeInit): KeyExchangeResponse {
      const appEph = decodeB64(init.ephemeral_public);
      const hpriv = x25519.utils.randomSecretKey();
      const hpub = x25519.getPublicKey(hpriv);
      const shared = x25519.scalarMult(hpriv, appEph);
      heraldKey = hkdf(sha256, shared, new Uint8Array(), HKDF_INFO, 32);
      const sig = ed25519.sign(
        kexTranscript(tagmaId, convId, appEph, hpub),
        deviceSecret,
      );
      return { ephemeral_public: encodeB64(hpub), signature: encodeB64(sig) };
    },
    postEnvelope(_c: string, envelope: Envelope): void {
      // Herald decrypts the app's request (dir=0).
      const pt = aeadDecrypt(
        heraldKey!,
        DIR_APP_TO_HERALD,
        envelope.sequence_n,
        decodeB64(envelope.ciphertext),
      );
      assertExists(pt, "herald must decrypt the app's envelope");
      const req = JSON.parse(new TextDecoder().decode(pt)) as TagmaRequest;
      lastRequest = req;
      receivedSeqs.push(envelope.sequence_n);
      const text = req.op === "send_message" ? req.text : "";
      // Herald encrypts a Finished reply (dir=1, its own counter) and feeds it
      // back to the channel via the SSE-demux path.
      const reply: TagmaReply = {
        kind: "event",
        event: { type: "finished", content: `echo:${text}` },
      };
      const seq = heraldSeq++;
      const ct = aeadEncrypt(
        heraldKey!,
        DIR_HERALD_TO_APP,
        seq,
        enc.encode(JSON.stringify(reply)),
      );
      channel!.enqueue({
        conversation_id: convId,
        sender: { kind: "agent", tagma_id: tagmaId },
        sequence_n: seq,
        trace_id: "trace",
        timestamp: new Date().toISOString(),
        ciphertext: encodeB64(ct),
      });
    },
  };
  return {
    agora,
    lesche,
    setChannel: (c: RelayChannel) => {
      channel = c;
    },
    receivedSeqs,
    lastRequest: () => lastRequest,
  };
}

Deno.test(
  "openRelayChannel + send round-trips against a mock herald",
  async () => {
    const tagmaId = "tagma-1";
    const convId = "conv-1";
    const userId = "user-1";
    const deviceSecret = ed25519.utils.randomSecretKey();
    const { agora, lesche, setChannel } = makeMock(
      deviceSecret,
      tagmaId,
      convId,
    );

    const channel = await openRelayChannel(
      agora as unknown as AgoraClient,
      lesche as unknown as LescheClient,
      tagmaId,
      userId,
    );
    assertEquals(channel.convId, convId);
    assertEquals(channel.tagmaId, tagmaId);
    setChannel(channel);

    await channel.send("hello");

    const iter = channel.replies();
    const first = await iter.next();
    assertEquals(first.done, false);
    const reply = first.value as TagmaReply;
    if (reply.kind !== "event")
      throw new Error(`expected event, got ${reply.kind}`);
    if (reply.event.type !== "finished") {
      throw new Error(`expected finished, got ${reply.event.type}`);
    }
    assertEquals(reply.event.content, "echo:hello");
    channel.close();
    assertEquals((await iter.next()).done, true);
  },
);

Deno.test("interrupt sends an interrupt op with a fresh req_id", async () => {
  const { agora, lesche, setChannel, lastRequest } = makeMock(
    ed25519.utils.randomSecretKey(),
    "tagma-i",
    "conv-i",
  );
  const channel = await openRelayChannel(
    agora as unknown as AgoraClient,
    lesche as unknown as LescheClient,
    "tagma-i",
    "u",
  );
  setChannel(channel);
  await channel.interrupt();
  const req = lastRequest();
  if (!req || req.op !== "interrupt") {
    throw new Error(`expected interrupt op, got ${JSON.stringify(req)}`);
  }
  channel.close();
});

Deno.test(
  "send increments sequence_n from 0 (the AEAD nonce counter)",
  async () => {
    const { agora, lesche, setChannel, receivedSeqs } = makeMock(
      ed25519.utils.randomSecretKey(),
      "tagma-s",
      "conv-s",
    );
    const channel = await openRelayChannel(
      agora as unknown as AgoraClient,
      lesche as unknown as LescheClient,
      "tagma-s",
      "u",
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
    const { agora, lesche, setChannel } = makeMock(
      ed25519.utils.randomSecretKey(),
      "tagma-d",
      "conv-d",
    );
    const channel = await openRelayChannel(
      agora as unknown as AgoraClient,
      lesche as unknown as LescheClient,
      "tagma-d",
      "u",
    );
    setChannel(channel);
    // A garbage-ciphertext envelope (the SSE demux would route this in) that
    // cannot decrypt under the session key.
    channel.enqueue({
      conversation_id: channel.convId,
      sender: { kind: "agent", tagma_id: "tagma-d" },
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
    const reply = first.value as TagmaReply;
    if (reply.kind !== "event" || reply.event.type !== "finished") {
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
    const { agora, lesche, setChannel } = makeMock(
      ed25519.utils.randomSecretKey(),
      "tagma-503",
      "conv-503",
    );
    const channel = await openRelayChannel(
      agora as unknown as AgoraClient,
      lesche as unknown as LescheClient,
      "tagma-503",
      "u",
    );
    setChannel(channel);
    // Swap postEnvelope to a 503 (the lesche returns 503 "tagma offline").
    const overridable = lesche as unknown as { postEnvelope: () => void };
    overridable.postEnvelope = () => {
      throw new AgoraApiError(503, "tagma is offline");
    };
    let caught: unknown;
    try {
      await channel.send("x");
    } catch (e) {
      caught = e;
    }
    if (!(caught instanceof AgoraApiError) || caught.status !== 503) {
      throw new Error(
        `expected AgoraApiError(503), got ${JSON.stringify(caught)}`,
      );
    }
    channel.close();
  },
);
