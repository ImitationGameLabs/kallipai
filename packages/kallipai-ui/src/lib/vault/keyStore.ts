/// <reference lib="dom" />

// Per-device vault key persistence.
// The vault key never leaves the device and is not extractable -- it lives in
// the browser's keystore, persisted across sessions in IndexedDB (a CryptoKey
// survives structured clone into IDB even though its raw material stays
// sealed). The default storage is the real IndexedDB; tests build instances
// over an injected in-memory storage to exercise the generate-or-load logic
// without a browser.

import { generateVaultKey } from "./crypto.ts";

const DB_NAME = "kallipai-vault";
const DB_VERSION = 1;
const STORE = "keys";
const KEY_ID = "vault";

/** Minimal persistence seam: one slot holding the device vault key. */
export interface VaultKeyStorage {
  get(): Promise<CryptoKey | null>;
  set(key: CryptoKey): Promise<void>;
}

/** IndexedDB-backed storage for the vault key slot. Lazily opens (and
 *  upgrade-creates) the `kallipai-vault` DB, mirroring the relay cache's
 *  single-store/DB-promise pattern. */
function idbVaultKeyStorage(): VaultKeyStorage {
  let dbPromise: Promise<IDBDatabase> | null = null;

  function db(): Promise<IDBDatabase> {
    if (dbPromise) return dbPromise;
    dbPromise = new Promise<IDBDatabase>((resolve, reject) => {
      if (typeof indexedDB === "undefined") {
        reject(new Error("IndexedDB unavailable"));
        return;
      }
      const req = indexedDB.open(DB_NAME, DB_VERSION);
      req.onupgradeneeded = () => {
        if (!req.result.objectStoreNames.contains(STORE)) {
          req.result.createObjectStore(STORE);
        }
      };
      req.onsuccess = () => resolve(req.result);
      req.onerror = () =>
        reject(req.error ?? new Error("indexedDB open failed"));
    });
    return dbPromise;
  }

  return {
    async get(): Promise<CryptoKey | null> {
      const d = await db();
      return new Promise((resolve, reject) => {
        const req = d
          .transaction(STORE, "readonly")
          .objectStore(STORE)
          .get(KEY_ID);
        req.onsuccess = () => resolve((req.result as CryptoKey) ?? null);
        req.onerror = () =>
          reject(req.error ?? new Error("indexedDB get failed"));
      });
    },
    async set(key: CryptoKey): Promise<void> {
      const d = await db();
      return new Promise((resolve, reject) => {
        const req = d
          .transaction(STORE, "readwrite")
          .objectStore(STORE)
          .put(key, KEY_ID);
        req.onsuccess = () => resolve();
        req.onerror = () =>
          reject(req.error ?? new Error("indexedDB put failed"));
      });
    },
  };
}

/** Generate-or-load front end for the device vault key. Construction is cheap
 *  and side-effect free; the IndexedDB round trip happens on first `ensure()`.
 *  A real browser session passes no arguments; tests inject their own storage. */
export class VaultKeyStore {
  readonly #storage: VaultKeyStorage;
  #cached: Promise<CryptoKey> | null = null;

  constructor(storage: VaultKeyStorage = idbVaultKeyStorage()) {
    this.#storage = storage;
  }

  /** Load the stored device key, generating and persisting a fresh one on
   *  first use. Resolved once per document (the promise is cached), so
   *  repeated callers share one read/write cycle and cannot race two keys
   *  into the slot within a document. */
  ensure(): Promise<CryptoKey> {
    this.#cached ??= (async () => {
      const existing = await this.#storage.get();
      if (existing) return existing;
      const fresh = await generateVaultKey();
      await this.#storage.set(fresh);
      return fresh;
    })();
    return this.#cached;
  }
}

/** Process-wide device vault key accessor used by the provider vault UI. */
export const deviceVault = new VaultKeyStore();
