// Wire DTOs mirroring the files service responses (kallipai-files/src/api/*).
// All ids are server-minted; `record_id` is the addressability handle.

/// The upload response: the minted record plus its content address
/// (identical bytes deduplicate to the same blob id).
export interface PutResponse {
  record_id: string;
  blob_id: string;
}

/// The delivery response: the recipient's own copy (zero data copied).
export interface SendResponse {
  record_id: string;
  blob_id: string;
  path: string;
}

/// One listed record. `created_at` is RFC 3339; `size` is blob bytes.
export interface FileEntryView {
  id: string;
  path: string;
  size: number;
  created_at: string;
}

/// The caller-selected slice: the private face (`self`) or the shared face.
export type ListSpace = "self" | "shared";

export interface ListOptions {
  space: ListSpace;
  /** Narrowing prefix relative to the slice (e.g. `inbox/`); never a
   * leading `/` (the client narrows, never re-points). */
  prefix?: string;
  /** Page size; the server clamps to its MAX_LIST_LIMIT. */
  limit?: number;
}
