// Reactive view of the persisted app config. Nav, the gate, and Settings read
// `configStore.value` (and `loaded`) so they update when a write lands. The
// store loads once at construction (best-effort; failures leave value=null)
// and is mutated by the setters below -- the retained offline creds and the
// notifications switch survive each other's writes, so connecting stays
// re-auth-free.
//
// Two load-state shapes: `loaded` (reactive, for the gate's skeleton branch)
// and `ready` (a one-shot promise, for the boot side-effect in RootLayout's
// onMount -- awaited there so boot runs exactly once, after the config settles,
// with no `booted` flag and no effect read-of-write).
import { clearConfig, loadConfig, saveConfig } from "./config.ts";
import type { OfflineModeConfig, PersistedConfig } from "./config.ts";

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
   * Set (first-time setup) or clear (forget tagma) the offline credentials.
   * The notifications switch survives either way; the mode is not stored, so
   * nothing here flips it.
   */
  async setOffline(config: OfflineModeConfig | null): Promise<void> {
    const next: PersistedConfig = {
      ...(this.value ?? {}),
      offline: config ?? undefined,
    };
    await saveConfig(next);
    this.value = next;
  }

  /**
   * Flip the notifications switch. Preserves the stored offline creds; the
   * write-through keeps the on-disk shape in sync so a reload restores the
   * toggle.
   */
  async setNotificationsEnabled(enabled: boolean): Promise<void> {
    const next: PersistedConfig = {
      ...(this.value ?? {}),
      notificationsEnabled: enabled,
    };
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
