// CatalogDetail: the open collection's unassigned rows, read per collection.
// The rows, the collection they belong to, and the request token that gates
// them live in one module: a token never leaves the state it guards, so the
// race it keeps out is testable without a page.
//
// The page hands in the open collection and the read; the effect that re-reads
// on a collection change stays on the page, where the reactive source is.

import type { AdminProfileRow } from "./client.ts";

export interface CatalogDetailOptions {
  /** The open collection's name, or null on the list route. */
  readonly collection: () => string | null;
  /** Read a collection's unassigned rows; undefined marks a failed read. */
  readonly readUnassigned: (
    collection: string,
  ) => Promise<AdminProfileRow[] | undefined>;
}

export interface CatalogDetail {
  /** The open collection's rows; null while the first read is in flight. */
  readonly rows: AdminProfileRow[] | null;
  /** The collection rows were read for (null on the list route). The page
   * renders the rows without it: this is the read's own bookkeeping, exposed
   * so a test can assert which collection a read belongs to. */
  readonly collection: string | null;
  /** Re-read the open collection's rows. A switch nulls them first. */
  refresh: () => Promise<void>;
}

export function createCatalogDetail(
  options: CatalogDetailOptions,
): CatalogDetail {
  let rows = $state<AdminProfileRow[] | null>(null);
  // Which collection the rows belong to. Plain state: the effect that calls
  // refresh must not track this bookkeeping and re-read twice.
  let forCollection: string | null = null;
  // The newest read's token: only it may land. A slow answer for a collection
  // that already left would otherwise repaint it.
  let request = 0;

  async function refresh(): Promise<void> {
    const ticket = ++request;
    const name = options.collection();
    if (name !== forCollection) {
      // A switch clears first: a slow or failed read must not leave the
      // previous collection's rows under the new detail.
      rows = null;
      forCollection = name;
    }
    if (name === null) return;
    const read = await options.readUnassigned(name);
    // A failed read keeps the rows it could not replace.
    if (read !== undefined && ticket === request) rows = read;
  }

  return {
    get rows() {
      return rows;
    },
    get collection() {
      // The bookkeeping account, not a page input: the rows are what the page
      // renders.
      return forCollection;
    },
    refresh,
  };
}
