// RelayChannel: the online chat data-plane transport. NOT a `Session` — it is a
// pure E2EE pipe over the archeion/lesche split. The pinned device key is TOFU
// from the archeion (control plane); the conversation, key exchange, and envelope
// relay run on the lesche (data plane). The browser opens a channel to a tagma
// (key exchange against the archeion-pinned key), then encrypts `TagmaRequest`s
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
  uuidV4,
} from "@kallipai/kallip-common";
import { LescheApiError } from "./types.ts";
import type {
  Envelope,
  FileAttachment,
  KeyExchangeInit,
  Participant,
  TagmaControl,
  TagmaReply,
  TagmaRequest,
} from "./types.ts";

/**
 * Open an E2EE channel to `tagmaId` for `userId`: resolve the conversation + run
 * the 1-RTT key exchange on the lesche, verify the responder's signature
 * against the archeion-pinned key (`pinnedKeyB64`, the standard-base64 Ed25519
 * public key the archeion's `GET /v1/archeion/tagmata/{id}` returns verbatim), and derive
 * the session key. The pinned key is fetched from the archeion by the caller (the
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

  /** Send a user message. Resolves, once the lesche accepts the envelope
   * (202), to the request's `req_id`: the caller can correlate the eventual
   * `error` reply to this send (the server echoes the same `req_id`). The
   * tagma's `message_accepted`/`error` reply flows through `replies`. */
  send(text: string, attachment?: FileAttachment): Promise<number> {
    const req_id = this.nextReqId++;
    // Network-layer retries (operator-final params): the POST may fail
    // transiently (relay down, socket hiccup); retry with exponential
    // backoff -- 5 attempts total, 1/2/4/8s gaps -- before surfacing the
    // failure. Client errors (4xx) are deterministic and never retried.
    // The 30s cap per attempt rides the shared fetch timeout (http.ts).
    return sendWithRetry(() =>
      this.sendRequest({
        op: "send_message",
        req_id,
        text,
        ...(attachment ? { attachment } : {}),
      }),
    ).then(() => req_id);
  }

  /** Request a batch of chat history (cursor-based). `after` = incremental
   * catch-up (rows newer than the rendered high-water mark); `before` =
   * scroll-up lazy load (rows older than the oldest id in view); both null =
   * the most recent `limit` rows (a first-time device). The matching rows and a
   * `history_batch_end` marker flow through `replies()`. Resolves to the
   * request's `req_id` once the envelope is accepted, so the caller can await
   * ITS marker on the reply stream (the marker echoes the same `req_id`).
   * The same encrypted envelope channel as a `TagmaRequest`; lesche is unaware. */
  history(opts: {
    after?: number | null;
    before?: number | null;
    limit?: number;
  }): Promise<number> {
    const req_id = this.nextReqId++;
    const ctrl: TagmaControl = {
      op: "history",
      req_id,
      after: opts.after ?? null,
      before: opts.before ?? null,
      limit: opts.limit ?? 50,
    };
    return this.sendControl(ctrl).then(() => req_id);
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
   * dropped. Does not close the underlying archeion SSE (owned by the demux). */
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
      trace_id: uuidV4(),
      timestamp: new Date().toISOString(),
      ciphertext: encodeB64(ciphertext),
    };
    await this.lesche.postEnvelope(this.conversationId, envelope);
  }
}

const SEND_RETRY_DELAYS_MS = [1_000, 2_000, 4_000, 8_000] as const;

/** Whether a send failure is worth retrying: transport errors (the fetch
 * threw -- network down, socket reset, timeout) and server-side 5xx yes;
 * client errors (4xx -- a rejected envelope is rejected for good) no.
 * Exported for tests. */
export function isRetryableSendError(e: unknown): boolean {
  if (e instanceof LescheApiError) return e.status >= 500;
  return true;
}

/** Retry an async send attempt with exponential backoff (5 attempts total:
 * 1/2/4/8s gaps). The last error propagates when the attempts are spent or
 * the failure is non-retryable. `sleep` is injectable so tests run
 * instantly. Module-level (not a method) for the same reason.
 *
 * A retried send re-POSTs the SAME req_id in a NEW envelope (fresh sequence,
 * fresh trace id): a lost 202 response means the server may already have
 * accepted the first attempt, so the retry can double-deliver -- the same
 * at-least-once contract the reconnect pending flush declares; the UI's
 * history_id dedup absorbs the duplicate at render time. */
export async function sendWithRetry<T>(
  attempt: () => Promise<T>,
  delays: readonly number[] = SEND_RETRY_DELAYS_MS,
  sleep: (ms: number) => Promise<void> = (ms) =>
    new Promise<void>((resolve) => setTimeout(resolve, ms)),
): Promise<T> {
  for (let i = 0; ; i++) {
    try {
      return await attempt();
    } catch (e) {
      const delay = delays[i];
      if (delay === undefined || !isRetryableSendError(e)) throw e;
      await sleep(delay);
    }
  }
}
