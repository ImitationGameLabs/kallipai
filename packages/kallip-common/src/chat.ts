// Shared chat wire types: the external chat-room vocabulary served by the
// tagma's projector onto BOTH transports — the direct (offline) SSE and the
// relayed (online) E2EE envelope. They mirror the serde DTOs in
// `crates/kallip_lesche_common/src/{message,event}.rs`. Lives here (not in
// kallip-lesche-client) so the direct client (@kallipai/kallip-client) and the
// relay client share one source of truth for the chat shape, with no offline ->
// online package dependency.

/** Why a failover chain ran out. Mirrors `event.rs::FailoverChainExhausted`
 * (serde `rename_all = "camelCase"`). */
export type FailoverChainExhausted =
  | "noFailoverConfigured"
  | "allBackupsExhausted"
  | "allCandidatesUnbuildable"
  | "allCandidatesInfeasible";

/** An authored message: conversation content that crosses the E2EE envelope
 * (online) or the plaintext direct SSE (offline) and is persisted in
 * chat_history (replayable on reconnect). serde tag = `type`, snake_case. There
 * is no streaming on this surface: `assistant_content` is a complete message.
 * Runtime signals (busy/idle, terminals, errors) do NOT ride this type — they
 * cross as a plaintext signal frame / `LescheEvent.tagma_signal`. */
export type AuthoredEvent = {
  readonly type: "assistant_content";
  readonly content: string;
};

/** A runtime signal: operator metadata (busy/idle presence, turn terminals,
 * errors) that crosses the plaintext signal channel. serde tag = `type`,
 * snake_case. Not persisted in chat_history and not replayed: a reconnect only
 * replays authored messages. */
export type SignalEvent =
  | { readonly type: "idle" }
  | { readonly type: "busy" }
  | { readonly type: "error"; readonly message: string }
  | { readonly type: "interrupted" }
  | { readonly type: "cancelled" }
  | {
      readonly type: "token_budget_exceeded";
      readonly consumed: number;
      readonly budget: number;
    }
  | { readonly type: "max_rounds_exceeded" }
  | {
      readonly type: "failover_chain_exhausted";
      readonly reason: FailoverChainExhausted;
      readonly detail: string;
    };

/** Responder -> app: either the result of a correlated op, or an unsolicited
 * authored event from the tagma's projector. serde tag = `kind`, snake_case.
 * Served on the direct external SSE (`authored` frames), the direct history
 * endpoint, and (encrypted) the relay envelope. Runtime signals do NOT ride the
 * `event` kind — they cross as a plaintext signal frame. */
export type TagmaReply =
  | {
      readonly kind: "message_accepted";
      readonly req_id: number;
      readonly queue_depth: number;
      readonly warning?: string;
      /** `chat_history.id` of the **inbound** row the tagma appended for this
       * user message. The direct path no longer stamps this (the projector's
       * published `user_message` frame is the promotion path); the relay path
       * historically carried it. `0`/absent means no row was recorded and must
       * not be used for dedup. The ack itself is never stored or replayed. */
      readonly history_id?: number;
      /** RFC 3339 send time of the inbound row. Absent on acks with no durable
       * row and on payloads serialized before the field existed. */
      readonly created_at?: string;
    }
  | { readonly kind: "interrupted"; readonly req_id: number }
  | {
      readonly kind: "error";
      readonly req_id: number;
      readonly status: number;
      readonly message: string;
    }
  | {
      readonly kind: "event";
      readonly event: AuthoredEvent;
      /** `chat_history.id` of the outbound row the projector appended for this
       * event; a stable id the app uses to order/dedup frames across batch replay
       * and live delivery. `0`/absent means the row was not recorded and must not
       * be used for dedup. */
      readonly history_id?: number;
      /** RFC 3339 send time of the outbound row. Absent on frames with no durable
       * row and on payloads serialized before the field existed. */
      readonly created_at?: string;
    }
  | {
      readonly kind: "user_message";
      readonly history_id: number;
      readonly text: string;
      /** RFC 3339 send time of the original inbound row. Absent on payloads
       * serialized before the field existed. */
      readonly created_at?: string;
    }
  | {
      readonly kind: "history_batch_end";
      readonly req_id: number;
      readonly count: number;
      readonly more: boolean;
    };
