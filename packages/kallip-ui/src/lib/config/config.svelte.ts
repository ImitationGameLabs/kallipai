// Reactive view of the persisted app config. Nav, the gate, and Settings read
// `configStore.value` (and `loaded`) so they update when a switch writes
// through. The store loads once at construction (best-effort; failures leave
// value=null, the online default) and is mutated by the two orthogonal
// mutators below -- neither of which destroys the other mode's state, so mode
// switching is non-destructive and re-auth-free in both directions.
//
// Two load-state shapes: `loaded` (reactive, for the gate's skeleton branch)
// and `ready` (a one-shot promise, for the boot side-effect in RootLayout's
// onMount -- awaited there so boot runs exactly once, after the config settles,
// with no `booted` flag and no effect read-of-write).
import { clearConfig, loadConfig, saveConfig } from "./config.ts";
import type { OfflineModeConfig, PersistedConfig } from "./config.ts";
import type { AppMode } from "./mode.ts";

class ConfigStore {
  value: PersistedConfig | null = $state(null);
  loaded = $state(false);
  readonly ready: Promise<void> = this.refresh().finally(() => {
    this.loaded = true;
  });

  private async refresh(): Promise<void> {
    try {
      this.value = await loadConfig();
    } catch {
      this.value = null;
    }
  }

  /**
   * Flip the active mode, preserving any retained offline creds. The write
   * through saveConfig keeps the on-disk shape in sync; the cookie-based online
   * session is untouched, so switching back is re-auth-free.
   */
  async setActiveMode(mode: AppMode): Promise<void> {
    const next: PersistedConfig = {
      ...(this.value ?? { activeMode: mode }),
      activeMode: mode,
    };
    await saveConfig(next);
    this.value = next;
  }

  /**
   * Set (first-time setup / new creds) or clear (forget daemon) the offline
   * credentials. Clearing also drops back to online -- offline mode is not
   * meaningful without creds -- so there is a single consistent forget path.
   * Setting creds preserves the current activeMode; the caller flips it with
   * setActiveMode (kept orthogonal so each method has one job).
   */
  async setOffline(config: OfflineModeConfig | null): Promise<void> {
    const next: PersistedConfig =
      config === null
        ? { activeMode: "online" }
        : { activeMode: this.value?.activeMode ?? "online", offline: config };
    await saveConfig(next);
    this.value = next;
  }

  /** Full reset (forget everything). */
  async clearValue(): Promise<void> {
    await clearConfig();
    this.value = null;
  }
}

export const configStore = new ConfigStore();
