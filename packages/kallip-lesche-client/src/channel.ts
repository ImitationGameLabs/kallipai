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
// Mirrors the Rust relay's `crates/platform/kallip-e2ee/src/lib.rs` + the
// lesche's `crates/platform/kallip-lesche/src/routes/conversations.rs`.

import { type LescheClient } from "./http.ts";
import {
  aeadDecrypt,
  aeadEncrypt,
  deriveSessionKey,
  DIR_INITIATOR_TO_RESPONDER,
  DIR_RESPONDER_TO_INITIATOR,
  generateEphemeralKeyPair,
  verifyKeyExchange,
} from "./crypto.ts";
import {
  decodeB64,
  encodeB64,
  participantIdForUser,
} from "@kallipai/kallip-common";
import type {
  Envelope,
  KeyExchangeInit,
  Participant,
  TagmaControl,
  TagmaReply,
  TagmaRequest,
} from "./types.ts";

/**
 * Open an E2EE channel to `tagmaId` for `userId`: resolve the conversation + run
 * the 1-RTT key exchange on the lesche, verify the responder's signature
 * against the agora-pinned key (`pinnedKeyB64`, the standard-base64 Ed25519
 * public key the agora's `GET /v1/tagmata/{id}` returns verbatim), and derive
 * the session key. The pinned key is fetched from the agora by the caller (the
 * control-plane client is not a dependency of this package); the caller passes
 * the base64 string as-is so no base64 helper leaks across the boundary.
 *
 * History is pull-based: the channel does NOT auto-request it; the UI store
 * hydrates its local cache and then sends a `TagmaControl::History`
 * (`after: maxRendered` for incremental, or `latest` for an empty cache) to
 * fetch what it is missing, drained through the normal `replies()` stream.
 * Throws if the tagma is offline / not owned / the signature fails to verify.
 */
export async function openRelayChannel(
  lesche: LescheClient,
  tagmaId: string,
  userId: string,
  userHandle: string,
  pinnedKeyB64: string,
): Promise<RelayChannel> {
  const pinnedKey = decodeB64(pinnedKeyB64);
  const { conversation_id: conversationId } =
    await lesche.createConversation(tagmaId);

  const { privateKey: initiatorPriv, publicKey: initiatorEph } =
    generateEphemeralKeyPair();
  const init: KeyExchangeInit = { ephemeral_public: encodeB64(initiatorEph) };
  const resp = await lesche.keyExchangeInit(conversationId, init);
  const responderEph = decodeB64(resp.ephemeral_public);
  const signature = decodeB64(resp.signature);
  if (
    !verifyKeyExchange(
      pinnedKey,
      tagmaId,
      conversationId,
      initiatorEph,
      responderEph,
      signature,
    )
  ) {
    throw new Error(
      "key-exchange signature failed to verify against the pinned key",
    );
  }
  const sessionKey = deriveSessionKey(initiatorPriv, responderEph);
  // The wire sender id is the opaque room-layer participant id (a deterministic
  // derivation from the user id), NOT the raw user id -- it must match the
  // Rust `Participant::id` (`ParticipantId::for_user`) byte-for-byte.
  const participantId = await participantIdForUser(userId);
  return new RelayChannel(
    lesche,
    conversationId,
    tagmaId,
    participantId,
    userHandle,
    sessionKey,
  );
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
  private readonly inbound: { sender: Participant; reply: TagmaReply }[] = [];
  private resolveDrain: (() => void) | null = null;
  private closed = false;
  private readonly pendingManage = new Map<
    number,
    {
      resolve: (v: { status: number; body: unknown }) => void;
      reject: (e: unknown) => void;
    }
  >();

  /** Assembled by [`openRelayChannel`]; do not construct directly — it skips
   * the key-exchange verification the factory performs. */
  constructor(
    private readonly lesche: LescheClient,
    readonly conversationId: string,
    readonly tagmaId: string,
    private readonly participantId: string,
    private readonly userHandle: string,
    private readonly sessionKey: Uint8Array,
  ) {}

  /** The local user's wire sender (the participant the app stamps on outbound
   * envelopes and on optimistic bubbles). `id` is the derived participant id. */
  get localParticipant(): Participant {
    return {
      id: this.participantId,
      kind: "human",
      handle: this.userHandle,
    };
  }

  /** Decrypt an inbound envelope and append its `TagmaReply` to the queue.
   * Called by the SSE demux. A ciphertext that fails to decrypt (wrong key,
   * tampering, wrong nonce) is dropped: the responder is the only legitimate
   * sender under `dir=1`, so a failure means corruption or a replay under the
   * wrong sequence, neither of which the app can recover. */
  enqueue(envelope: Envelope): void {
    if (this.closed) return;
    if (envelope.channel_id !== this.conversationId) return;
    const ciphertext = decodeB64(envelope.ciphertext);
    const plaintext = aeadDecrypt(
      this.sessionKey,
      DIR_RESPONDER_TO_INITIATOR,
      envelope.sequence_n,
      ciphertext,
    );
    if (plaintext === null) {
      if (this.decryptFailures++ === 0) {
        console.warn(
          `[RelayChannel ${this.conversationId}] dropped an undecryptable inbound envelope (seq=${envelope.sequence_n})`,
        );
      }
      return;
    }
    const reply = JSON.parse(new TextDecoder().decode(plaintext)) as TagmaReply;
    if (reply.kind === "manage_result") {
      const pending = this.pendingManage.get(reply.req_id);
      if (pending) {
        this.pendingManage.delete(reply.req_id);
        pending.resolve({ status: reply.status, body: reply.body });
        return;
      }
    }
    this.inbound.push({ sender: envelope.sender, reply });
    this.resolveDrain?.();
  }

  /** The decrypted `{sender, reply}` stream. The sender is the relay-authenticated
   * envelope sender (the tagma for outbound content, the user for the inbound
   * echo); it is no longer discarded. Ends when `close()` is called. The channel
   * is a pure transport: it does NOT dedup — ordering/dedup by `history_id` is
   * the UI store's job. */
  async *replies(): AsyncGenerator<{ sender: Participant; reply: TagmaReply }> {
    while (!this.closed) {
      while (this.inbound.length > 0) {
        yield this.inbound.shift()!;
      }
      if (this.closed) break;
      await new Promise<void>((resolve) => {
        this.resolveDrain = resolve;
      });
      this.resolveDrain = null;
    }
  }

  /** Send a user message. Resolves once the lesche accepts the envelope (202);
   * the tagma's `message_accepted`/`error` reply flows through `replies`. */
  send(text: string): Promise<void> {
    return this.sendRequest({
      op: "send_message",
      req_id: this.nextReqId++,
      text,
    });
  }

  /** Request a batch of chat history (cursor-based). `after` = incremental
   * catch-up (rows newer than the rendered high-water mark); `before` =
   * scroll-up lazy load (rows older than the oldest id in view); both null =
   * the most recent `limit` rows (a first-time device). The matching rows and a
   * `history_batch_end` marker flow through `replies()`. The same encrypted
   * envelope channel as a `TagmaRequest`; lesche is unaware. */
  history(opts: {
    after?: number | null;
    before?: number | null;
    limit?: number;
  }): Promise<void> {
    const ctrl: TagmaControl = {
      op: "history",
      req_id: this.nextReqId++,
      after: opts.after ?? null,
      before: opts.before ?? null,
      limit: opts.limit ?? 50,
    };
    return this.sendControl(ctrl);
  }

  manage(
    method: string,
    path: string,
    body: unknown = null,
  ): Promise<{ status: number; body: unknown }> {
    const req_id = this.nextReqId++;
    const ctrl: TagmaControl = { op: "manage", req_id, method, path, body };
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pendingManage.delete(req_id);
        reject(new Error(`manage timeout: ${method} ${path}`));
      }, 15_000);
      this.pendingManage.set(req_id, {
        resolve: (v) => {
          clearTimeout(timer);
          resolve(v);
        },
        reject: (e) => {
          clearTimeout(timer);
          reject(e);
        },
      });
      this.sendControl(ctrl).catch((e) => {
        clearTimeout(timer);
        this.pendingManage.delete(req_id);
        reject(e);
      });
    });
  }

  /** Stop the channel. The `replies` generator ends; further enqueues are
   * dropped. Does not close the underlying agora SSE (owned by the demux). */
  close(): void {
    this.closed = true;
    for (const { reject } of this.pendingManage.values()) {
      reject(new Error("channel closed"));
    }
    this.pendingManage.clear();
    this.resolveDrain?.();
    this.resolveDrain = null;
  }

  private sendRequest(req: TagmaRequest): Promise<void> {
    return this.sendPayload(req);
  }

  private sendControl(ctrl: TagmaControl): Promise<void> {
    return this.sendPayload(ctrl);
  }

  /** Encrypt + POST one app->tagma payload (a `TagmaRequest` that drives the
   * agent, or a `TagmaControl` plumbing op). Both share the envelope channel;
   * the relay dispatches by the `op` discriminant. */
  private async sendPayload(
    payload: TagmaRequest | TagmaControl,
  ): Promise<void> {
    const plaintext = new TextEncoder().encode(JSON.stringify(payload));
    const sequence_n = this.sendSeq++;
    const ciphertext = aeadEncrypt(
      this.sessionKey,
      DIR_INITIATOR_TO_RESPONDER,
      sequence_n,
      plaintext,
    );
    const sender: Participant = this.localParticipant;
    const envelope: Envelope = {
      channel_id: this.conversationId,
      sender,
      sequence_n,
      trace_id: crypto.randomUUID(),
      timestamp: new Date().toISOString(),
      ciphertext: encodeB64(ciphertext),
    };
    await this.lesche.postEnvelope(this.conversationId, envelope);
  }
}
