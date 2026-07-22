// Persisted app mode + offline tagma credentials. Online (agora) auth is a
// browser session cookie (see @kallipai/kallip-agora-client/http.ts), so it is
// never persisted here -- the only stored state is which mode is active and the
// offline tagma creds. Both are retained across mode switches so switching is
// non-destructive (re-auth-free in both directions); flipping the active mode
// never destroys the other side's credentials.
//
// The only module that decides which storage backend is used is the app
// bootstrap (initConfigStorage); everything else calls loadConfig/saveConfig/
// clearConfig. Storage is a raw string KV; (de)serialization and validation
// live here so the wipe rule ("anything we do not recognize gets cleared") has
// one implementation. Pre-release, per-browser: we do not migrate legacy
// shapes, we wipe them (project stance: prefer bold breaking changes over
// compat shims).

import type { AppMode } from "./mode.ts";
import type { ConfigStorage } from "./storage.ts";
import { localStorageConfigStorage } from "./storage.ts";

/** Offline tagma credentials. Online auth has no stored equivalent. */
export interface OfflineModeConfig {
  readonly tagmaUrl: string;
  readonly authToken: string;
}

/**
 * The persisted app state: which mode is active, plus retained offline creds.
 * `offline` is optional (first-time online users have none); when present it is
 * reused on every switch back to offline mode without re-entry.
 */
export interface PersistedConfig {
  readonly activeMode: AppMode;
  readonly offline?: OfflineModeConfig;
}

let storage: ConfigStorage = localStorageConfigStorage;

/** Inject the storage backend. Called once at app bootstrap. */
export function initConfigStorage(s: ConfigStorage): void {
  storage = s;
}

/**
 * Load and validate. A blob with a valid `activeMode` passes through unchanged.
 * Anything else -- a legacy pre-redesign shape, a corrupt blob, an
 * manually-edited value -- is cleared and treated as the online default (null),
 * so a stale/corrupt value can never drive `modeOf` astray. Empty storage is
 * the same null, but is NOT cleared (no spurious write on a fresh install).
 */
export async function loadConfig(): Promise<PersistedConfig | null> {
  const raw = await storage.load();
  if (raw === null) return null;
  const parsed = safeParse(raw);
  if (parsed && isValid(parsed)) return parsed;
  await storage.clear();
  return null;
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

// Structural guard: a value is a PersistedConfig iff it carries a known
// activeMode and, when `offline` creds are present, they have the expected
// string shape. A legacy shape (e.g. the pre-rename `daemonUrl` field) fails
// here and is wiped per the rule above, rather than loading with an undefined
// URL; the connect path still re-validates creds by actually dialing the tagma.
function isValid(value: unknown): value is PersistedConfig {
  if (typeof value !== "object" || value === null) return false;
  const mode = (value as { activeMode?: unknown }).activeMode;
  if (mode !== "online" && mode !== "offline") return false;
  const offline = (value as { offline?: unknown }).offline;
  if (offline !== undefined) {
    if (typeof offline !== "object" || offline === null) return false;
    const o = offline as { tagmaUrl?: unknown; authToken?: unknown };
    if (typeof o.tagmaUrl !== "string" || typeof o.authToken !== "string") {
      return false;
    }
  }
  return true;
}
