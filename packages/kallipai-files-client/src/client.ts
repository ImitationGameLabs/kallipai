// The files service's verbs, as effects, plus the promise face the UI awaits
// (`FilesClient`). The verbs mirror the server routes
// (kallipai-files/src/state.rs): streaming upload (PUT ?path=), download (GET
// /{id}), metadata (HEAD /{id}), listing (GET ?space=), delivery to another
// principal's inbox (POST /{id}/send), and deletion (DELETE /{id}).

import { runClientEffect } from "@kallipai/kallipai-common";
import { Effect } from "effect";

import { type FilesFailure, filesFetch, readBytes, readJson } from "./http.ts";
import type {
  FileEntryView,
  ListOptions,
  PutResponse,
  SendResponse,
} from "./types.ts";

/** Upload `blob` to `path` (a full space path such as
 * `/users/{user}/shared/report.pdf`). Returns the minted record. */
export function putRecord(
  base: string,
  path: string,
  blob: Blob | ArrayBuffer | Uint8Array,
): Effect.Effect<PutResponse, FilesFailure> {
  // `BufferSource` here is the caller's typed-array or buffer form;
  // `fetch` accepts it as a body, but the DOM `BodyInit` union is
  // spelled through `XMLHttpRequestBodyInit`, which Deno's DOM lib does
  // not spread to the generic typed arrays.
  const body = blob as BodyInit;
  const query = new URLSearchParams({ path });
  return filesFetch(base, `?${query}`, {
    method: "PUT",
    body,
  }).pipe(Effect.flatMap(readJson<PutResponse>));
}

/** Download the record's bytes. The server streams and honors a single
 * range; this client reads the whole body. */
export function getRecord(
  base: string,
  id: string,
): Effect.Effect<ArrayBuffer, FilesFailure> {
  return filesFetch(base, `/${id}`, { method: "GET" }).pipe(
    Effect.flatMap(readBytes),
  );
}

/**
 * Metadata probe: true when the record exists and the caller may read it. A
 * 404 answers false -- that is the question the probe asks. Any other
 * refusal (403 above all: the record may exist and stay unreadable) passes
 * through as `FilesApiError` carrying the status: a HEAD answer has no
 * body, so the status line is all the service can say. A call that
 * produced no answer fails as a dead exchange.
 */
export function headRecord(
  base: string,
  id: string,
): Effect.Effect<boolean, FilesFailure> {
  return filesFetch(base, `/${id}`, { method: "HEAD" }).pipe(
    Effect.map(() => true),
    Effect.catchTag("FilesApiError", (failure) =>
      failure.status === 404 ? Effect.succeed(false) : Effect.fail(failure),
    ),
  );
}

/** List the caller's slice of the files space. */
export function listRecords(
  base: string,
  options: ListOptions,
): Effect.Effect<FileEntryView[], FilesFailure> {
  const query = new URLSearchParams({ space: options.space });
  if (options.prefix !== undefined) query.set("prefix", options.prefix);
  if (options.limit !== undefined) query.set("limit", String(options.limit));
  return filesFetch(base, `?${query}`, { method: "GET" }).pipe(
    Effect.flatMap(readJson<FileEntryView[]>),
  );
}

/** Deliver a zero-copy copy of the record into a recipient's inbox. Exactly
 * one of `toUser` / `toTagma` must be set (the server rejects anything else).
 * Returns the recipient's copy handle. */
export function sendRecord(
  base: string,
  id: string,
  target: { toUser?: string; toTagma?: string },
): Effect.Effect<SendResponse, FilesFailure> {
  return filesFetch(base, `/${id}/send`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      to_user: target.toUser ?? null,
      to_tagma: target.toTagma ?? null,
    }),
  }).pipe(Effect.flatMap(readJson<SendResponse>));
}

/** Delete the record: the server releases the reference (a zeroed refcount
 * only stamps `freed_at`; the GC unlinks later). 204 with no body on
 * success. */
export function deleteRecord(
  base: string,
  id: string,
): Effect.Effect<void, FilesFailure> {
  return filesFetch(base, `/${id}`, { method: "DELETE" }).pipe(
    Effect.map(() => undefined),
  );
}

/**
 * The promise face of the verbs above, as this package offers it to consumers
 * (`kallipai-ui` is today's): one `runClientEffect` per call. Behaviour lives
 * in the verbs; a caller that can hold an Effect uses them directly. Do not
 * add behavior here. A rejection is the failure channel itself:
 * `FilesApiError` for a refusal, `TransportError` for a dead exchange.
 */
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

  put(
    path: string,
    blob: Blob | ArrayBuffer | Uint8Array,
  ): Promise<PutResponse> {
    return runClientEffect(putRecord(this.base, path, blob));
  }

  get(id: string): Promise<ArrayBuffer> {
    return runClientEffect(getRecord(this.base, id));
  }

  head(id: string): Promise<boolean> {
    return runClientEffect(headRecord(this.base, id));
  }

  list(options: ListOptions): Promise<FileEntryView[]> {
    return runClientEffect(listRecords(this.base, options));
  }

  send(
    id: string,
    target: { toUser?: string; toTagma?: string },
  ): Promise<SendResponse> {
    return runClientEffect(sendRecord(this.base, id, target));
  }

  delete(id: string): Promise<void> {
    return runClientEffect(deleteRecord(this.base, id));
  }
}
