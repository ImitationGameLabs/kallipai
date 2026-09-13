// The thin browser client for the files service. One class, six verbs,
// mirroring the server routes (kallip-files/src/state.rs): streaming upload
// (PUT ?path=), download (GET /{id}), metadata (HEAD /{id}), listing
// (GET ?space=), delivery to another principal's inbox (POST /{id}/send),
// and deletion (DELETE /{id}).

import { filesFetch } from "./http.ts";
import type {
  FileEntryView,
  ListOptions,
  PutResponse,
  SendResponse,
} from "./types.ts";

export class FilesClient {
  /** `base` is the full edge base, /v1/files included (the edge strips
   * the /v1/files prefix before proxying -- the service name is the
   * resource segment). Paths here are pure tails: `/{id}`, `/{id}/send`,
   * `?query`. */
  private readonly base: string;

  constructor(base = "") {
    // Trim a trailing slash so `${base}${path}` stays single-slash.
    this.base = base.replace(/\/+$/, "");
  }

  /** Upload `blob` to `path` (a full space path such as
   * `/users/{user}/shared/report.pdf`). Returns the minted record. */
  async put(
    path: string,
    blob: Blob | ArrayBuffer | Uint8Array,
  ): Promise<PutResponse> {
    const query = new URLSearchParams({ path });
    const response = await filesFetch(this.base, `?${query}`, {
      method: "PUT",
      body: blob as Blob,
    });
    return response.json();
  }

  /** Download the record's bytes. The server streams and honors a single
   * range; the browser client reads the whole body. */
  async get(id: string): Promise<ArrayBuffer> {
    const response = await filesFetch(this.base, `/${id}`, {
      method: "GET",
    });
    return response.arrayBuffer();
  }

  /** Metadata probe: true when the record exists and is readable (the ACL
   * decision rides the same route). */
  async head(id: string): Promise<boolean> {
    const response = await filesFetch(this.base, `/${id}`, {
      method: "HEAD",
    });
    return response.ok;
  }

  /** List the caller's slice of the files space. */
  async list(options: ListOptions): Promise<FileEntryView[]> {
    const query = new URLSearchParams({ space: options.space });
    if (options.prefix !== undefined) query.set("prefix", options.prefix);
    if (options.limit !== undefined) query.set("limit", String(options.limit));
    const response = await filesFetch(this.base, `?${query}`, {
      method: "GET",
    });
    return response.json();
  }

  /** Deliver a zero-copy copy of the record into a recipient's inbox.
   * Exactly one of `toUser` / `toTagma` must be set (the server rejects
   * anything else). Returns the recipient's copy handle. */
  async send(
    id: string,
    target: { toUser?: string; toTagma?: string },
  ): Promise<SendResponse> {
    const response = await filesFetch(this.base, `/${id}/send`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        to_user: target.toUser ?? null,
        to_tagma: target.toTagma ?? null,
      }),
    });
    return response.json();
  }

  /** Delete the record: the server releases the reference (a zeroed
   * refcount only stamps `freed_at`; the GC unlinks later). 204 with no
   * body on success. */
  async delete(id: string): Promise<void> {
    await filesFetch(this.base, `/${id}`, {
      method: "DELETE",
    });
  }
}
