// Pluggable raw-string persistence for the app config. The web app backs this
// with localStorage; the Tauri app will back it with secure storage once the
// plugin is wired (Phase 4). Both implementations are injected at app bootstrap
// via initConfigStorage(), so this package never imports a storage backend
// directly -- the future split-out of an app-core package is a one-adapter
// change.
//
// The interface is a raw string KV (load/save/clear): serialization and
// validation belong to config.ts so the wipe rule has one implementation.
// Async because the Tauri stronghold/store plugins are async; localStorage is
// wrapped in resolved Promises for parity.

export interface ConfigStorage {
  load(): Promise<string | null>;
  save(raw: string): Promise<void>;
  clear(): Promise<void>;
}

const KEY = "kallip:connection";

export const localStorageConfigStorage: ConfigStorage = {
  load: () => Promise.resolve(localStorage.getItem(KEY)),
  save: (raw) => Promise.resolve(localStorage.setItem(KEY, raw)),
  clear: () => Promise.resolve(localStorage.removeItem(KEY)),
};
