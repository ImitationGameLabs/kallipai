// Attachment upload state, shared between the composer's file button and
// the attachment bar. Pure data + pure predicates: the upload orchestration
// lives with the page (it owns the files client and the send flow), the bar
// renders, neither owns the other.

/** The client-side cap for a single file. Mirrors the server default
 * (KALLIPAI_FILES_MAX_BODY_SIZE_MB); a server-side 413 remains the authority
 * if the server is configured tighter. */
export const MAX_ATTACHMENT_BYTES = 100 * 1024 * 1024;

/** The client-side key for one picked file (stable across re-renders). */
export interface AttachmentItem {
  id: string;
  file: File;
  name: string;
  size: number;
  /** uploading: PUT in flight; ready: record minted; failed: retryable
   * transport/server error; too_large: rejected before any wire traffic. */
  status: "uploading" | "ready" | "failed" | "too_large";
  /** The minted record id once status is `ready` (the send-time handle). */
  recordId?: string;
}

/** A fresh `uploading` item from a picked file. */
export function newAttachment(seq: number, file: File): AttachmentItem {
  return {
    id: `att-${seq}`,
    file,
    name: file.name,
    size: file.size,
    status: "uploading",
  };
}

/** Pre-flight: too large is decided locally, no wire traffic. */
export function isTooLarge(item: AttachmentItem): boolean {
  return item.size > MAX_ATTACHMENT_BYTES;
}

/** The send gate: every attachment reached `ready` (an empty list sends). */
export function allReady(items: AttachmentItem[]): boolean {
  return items.every((item) => item.status === "ready");
}
