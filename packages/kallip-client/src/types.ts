// Wire types for the tagma HTTP surface the browser frontend consumes. The
// counterpart Rust serde DTOs live in kallip-common and kallip-tagma; field
// names are snake_case here (matching serde), and every base64 string is
// STANDARD base64 (padded, +//).

import type { HistoryEntry } from "@kallipai/kallip-common";

/** `GET /agents/root` -- the tagma's single root agent (always present after
 * startup). `id` binds the transport; `conversation_id` (present only on the
 * root summary, when the tagma is enrolled) is the shared key the offline and
 * online paths use for the IndexedDB cache + history pulls. */
export interface WireAgentSummary {
  readonly id: string;
  readonly workspace_root?: string;
  readonly state: "idle" | "busy" | "faulted";
  readonly created_by?: string;
  readonly role: string;
  readonly description?: string;
  readonly activity?: string;
  readonly faulted_reason?: string | null;
  readonly conversation_id?: string;
}

/** `POST /agents/{id}/message` -- queue-depth feedback for an inbound user
 * message. The direct path carries no `message_accepted` ack, so this is the
 * only response the send path observes. */
export interface MessageResponse {
  readonly queue_depth: number;
  readonly warning?: string;
}

/** `GET /agents/{id}/external/history` -- a cursor-driven history window for
 * the direct (offline) path. `rows` are decoded `HistoryEntry` frames (the
 * sender paired with an authored `event` / `user_message` echo); `more` is true
 * only for paginated (`after`/`before`) queries that returned a full page.
 * Mirrors the relay `TagmaControl::History` shape. */
export interface ExternalHistoryResponse {
  /** History entries: the sender paired with the content-only reply (mirrors
   * the live `{sender, body}` shape). */
  readonly rows: readonly HistoryEntry[];
  readonly more: boolean;
}
