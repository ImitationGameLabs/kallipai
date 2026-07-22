// RelayChannel: the online chat data-plane transport. NOT a `Session` — it is a
// pure E2EE pipe over the agora/lesche split. The pinned device key is TOFU
// from the agora (control plane); the conversation, key exchange, and envelope
// relay run on the lesche (data plane). The browser opens a channel to a tagma
// (key exchange against the agora-pinned key), then encrypts `TagmaRequest`s
// into lesche envelopes and decrypts inbound `TagmaReply` envelopes that the SSE
// demux routes to `enqueue`. The channel does NOT interpret `TagmaReply`
// semantics; that is the UI store's job (see kallip-ui's channel transcript
// reducer).
//
// Mirrors the herald's `crates/kallip-herald/src/e2e.rs` + the lesche's
// `crates/kallip-lesche/src/routes/conversations.rs`.

import { type AgoraClient, type LescheClient } from "./http.ts";
import {
  aeadDecrypt,
  aeadEncrypt,
  deriveSessionKey,
  DIR_APP_TO_HERALD,
  DIR_HERALD_TO_APP,
  generateEphemeralKeyPair,
  verifyKeyExchange,
} from "./crypto.ts";
import { decodeB64, encodeB64 } from "./base64.ts";
import type {
  Envelope,
  KeyExchangeInit,
  Participant,
  TagmaReply,
  TagmaRequest,
} from "./types.ts";

/**
 * Open an E2EE channel to `tagmaId` for `userId`: fetch the pinned key from the
 * agora, resolve the conversation + run the 1-RTT key exchange on the lesche,
 * verify the herald's signature against the agora-pinned key, and derive the
 * session key. The channel keeps the lesche client (only `postEnvelope` is
 * needed after open). Throws if the tagma is offline / not owned / the signature
 * fails to verify.
 */
export async function openRelayChannel(
  agora: AgoraClient,
  lesche: LescheClient,
  tagmaId: string,
  userId: string,
): Promise<RelayChannel> {
  const info = await agora.getTagma(tagmaId);
  const pinnedKey = decodeB64(info.pinned_public_key);
  const { conversation_id: convId } = await lesche.createConversation(tagmaId);

  const { privateKey: appPriv, publicKey: appEph } = generateEphemeralKeyPair();
  const init: KeyExchangeInit = { ephemeral_public: encodeB64(appEph) };
  const resp = await lesche.keyExchangeInit(convId, init);
  const heraldEph = decodeB64(resp.ephemeral_public);
  const signature = decodeB64(resp.signature);
  if (
    !verifyKeyExchange(pinnedKey, tagmaId, convId, appEph, heraldEph, signature)
  ) {
    throw new Error(
      "key-exchange signature failed to verify against the pinned key",
    );
  }
  const sessionKey = deriveSessionKey(appPriv, heraldEph);

  return new RelayChannel(lesche, convId, tagmaId, userId, sessionKey);
}

/**
 * One E2EE channel to a tagma. Outbound: encrypt a `TagmaRequest` into an
 * envelope and POST it. Inbound: the SSE demux feeds envelopes to `enqueue`;
 * they are decrypted and the `TagmaReply` is yielded on `replies`. The channel
 * holds the AEAD session key and the app's per-sender sequence counter.
 */
export class RelayChannel {
  private sendSeq = 0;
  private nextReqId = 1;
  private decryptFailures = 0;
  private readonly inbound: Envelope[] = [];
  private resolveDrain: (() => void) | null = null;
  private closed = false;

  /** Assembled by [`openRelayChannel`]; do not construct directly — it skips
   * the key-exchange verification the factory performs. */
  constructor(
    private readonly lesche: LescheClient,
    readonly convId: string,
    readonly tagmaId: string,
    private readonly userId: string,
    private readonly sessionKey: Uint8Array,
  ) {}

  /** Decrypt an inbound envelope and append its `TagmaReply` to the queue.
   * Called by the SSE demux. A ciphertext that fails to decrypt (wrong key,
   * tampering, wrong nonce) is dropped: the herald is the only legitimate
   * sender under `dir=1`, so a failure means corruption or a replay under the
   * wrong sequence, neither of which the app can recover. */
  enqueue(envelope: Envelope): void {
    if (this.closed) return; // a late envelope after close is dropped
    if (envelope.conversation_id !== this.convId) return;
    // The reply flows out via `replies` once a consumer is draining.
    this.inbound.push(envelope);
    this.resolveDrain?.();
  }

  /** The decrypted `TagmaReply` stream. Ends when `close()` is called. */
  async *replies(): AsyncGenerator<TagmaReply> {
    while (!this.closed) {
      while (this.inbound.length > 0) {
        const envelope = this.inbound.shift()!;
        const ciphertext = decodeB64(envelope.ciphertext);
        const plaintext = aeadDecrypt(
          this.sessionKey,
          DIR_HERALD_TO_APP,
          envelope.sequence_n,
          ciphertext,
        );
        if (plaintext === null) {
          // A failure under dir=1 means corruption, tampering, or a wrong
          // session key. The first one logs (so a key mismatch during bring-up
          // is not a silent stall); subsequent ones stay quiet to avoid log
          // spam from a sustained mismatch.
          if (this.decryptFailures++ === 0) {
            console.warn(
              `[RelayChannel ${this.convId}] dropped an undecryptable inbound envelope (seq=${envelope.sequence_n}); a persistent failure means the session key is wrong.`,
            );
          }
          continue;
        }
        yield JSON.parse(new TextDecoder().decode(plaintext)) as TagmaReply;
      }
      if (this.closed) break;
      // Wait for the next inbound envelope (or close).
      await new Promise<void>((resolve) => {
        this.resolveDrain = resolve;
      });
      this.resolveDrain = null;
    }
  }

  /** Send a user message. Resolves once the lesche accepts the envelope (202);
   * the herald's `message_accepted`/`error` reply flows through `replies`. */
  send(text: string): Promise<void> {
    return this.sendRequest({
      op: "send_message",
      req_id: this.nextReqId++,
      text,
    });
  }

  /** Interrupt the in-flight turn. */
  interrupt(): Promise<void> {
    return this.sendRequest({ op: "interrupt", req_id: this.nextReqId++ });
  }

  /** Stop the channel. The `replies` generator ends; further enqueues are
   * dropped. Does not close the underlying agora SSE (owned by the demux). */
  close(): void {
    this.closed = true;
    this.resolveDrain?.();
    this.resolveDrain = null;
  }

  private async sendRequest(req: TagmaRequest): Promise<void> {
    const plaintext = new TextEncoder().encode(JSON.stringify(req));
    const sequence_n = this.sendSeq++;
    const ciphertext = aeadEncrypt(
      this.sessionKey,
      DIR_APP_TO_HERALD,
      sequence_n,
      plaintext,
    );
    const sender: Participant = { kind: "user", user_id: this.userId };
    const envelope: Envelope = {
      conversation_id: this.convId,
      sender,
      sequence_n,
      trace_id: crypto.randomUUID(),
      timestamp: new Date().toISOString(),
      ciphertext: encodeB64(ciphertext),
    };
    await this.lesche.postEnvelope(this.convId, envelope);
  }
}
