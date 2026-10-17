// Persisted browser state: the retained offline tagma credentials and the
// notifications switch. Online (archeion) auth is a browser session cookie
// (see @kallipai/kallipai-archeion-client/http.ts), so it is never
// persisted here, and neither is the app mode -- that belongs to the shell,
// which names its two values in lib/config/mode.ts.
//
// The only module that decides which storage backend is used is the app
// bootstrap (initConfigStorage); everything else calls loadConfig/saveConfig/
// clearConfig. Storage is a raw string KV; (de)serialization and validation
// live here so validation, and the drop of fields this build does not
// know, have one implementation. A blob whose known fields are malformed
// is wiped; unknown top-level keys are ignored and dropped on the next
// write. Pre-release, per-browser: we do not migrate legacy shapes, we
// wipe them (project stance: prefer bold breaking changes over compat
// shims).

import type { ConfigStorage } from "./storage.ts";
import { localStorageConfigStorage } from "./storage.ts";

/** Offline tagma credentials. Online auth has no stored equivalent. */
export interface OfflineModeConfig {
  readonly tagmaUrl: string;
  readonly authToken: string;
}

/**
 * The persisted app state: retained offline creds and the notifications
 * switch. `offline` is optional (first-time online users have none); when
 * present it is reused on the next connect without re-entry.
 * `notificationsEnabled` absent = off, the safe default for configs written
 * before the switch existed.
 */
export interface PersistedConfig {
  readonly offline?: OfflineModeConfig;
  readonly notificationsEnabled?: boolean;
}

let storage: ConfigStorage = localStorageConfigStorage;

/** Inject the storage backend. Called once at app bootstrap. */
export function initConfigStorage(s: ConfigStorage): void {
  storage = s;
}

/**
 * Load and validate. A blob whose known fields have the expected shape
 * loads with just those fields -- a field this build does not store (an
 * older build's `activeMode`) is dropped here, so the next write does not
 * carry it forward, and is not a reason to wipe the blob. Anything else --
 * a corrupt blob, a manually-edited value -- is cleared and treated as the
 * online default (null). Empty storage is the same null, but is NOT
 * cleared (no spurious write on a fresh install).
 */
export async function loadConfig(): Promise<PersistedConfig | null> {
  const raw = await storage.load();
  if (raw === null) return null;
  const parsed = safeParse(raw);
  if (parsed && isValid(parsed)) return pickKnown(parsed);
  await storage.clear();
  return null;
}

/**
 * Keep only what this build persists, at both levels: an unknown top-level
 * key drops, and so does an unknown key inside `offline`. The conditional
 * spreads keep an absent field absent -- a key written as `undefined` would
 * still land in the blob.
 */
function pickKnown(config: PersistedConfig): PersistedConfig {
  const offline = config.offline;
  const notifications = config.notificationsEnabled;
  return {
    ...(offline === undefined
      ? {}
      : {
          offline: { tagmaUrl: offline.tagmaUrl, authToken: offline.authToken },
        }),
    ...(notifications === undefined
      ? {}
      : { notificationsEnabled: notifications }),
  };
}

export async function saveConfig(config: PersistedConfig): Promise<void> {
  await storage.save(JSON.stringify(config));
}

export async function clearConfig(): Promise<void> {
  await storage.clear();
}

function safeParse(raw: string): unknown {
  try {
    return JSON.parse(raw);
  } catch {
    return null;
  }
}

// Structural guard: when `offline` is present both credential fields must be
// strings, and when `notificationsEnabled` is present it must be a boolean.
// A blob that fails is wiped and loads as the online default. A blob whose
// top-level keys are all unknown passes: it loads as an empty config and is
// not wiped. The connect path re-validates creds by dialing.
function isValid(value: unknown): value is PersistedConfig {
  if (typeof value !== "object" || value === null) return false;
  const offline = (value as { offline?: unknown }).offline;
  if (offline !== undefined) {
    if (typeof offline !== "object" || offline === null) return false;
    const o = offline as { tagmaUrl?: unknown; authToken?: unknown };
    if (typeof o.tagmaUrl !== "string" || typeof o.authToken !== "string") {
      return false;
    }
  }
  const notifications = (value as { notificationsEnabled?: unknown })
    .notificationsEnabled;
  if (notifications !== undefined && typeof notifications !== "boolean") {
    return false;
  }
  return true;
}
