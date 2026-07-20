// The app has two modes, selected by the user and recorded in the persisted
// config's `activeMode`:
//   - "offline" connects straight to a kallip daemon on the user's machine/LAN
//     (no identity, no tagmata);
//   - "online" is agora passkey auth + tagmata management.
// Centralizing the derivation here keeps the gate (`appGateDecision`), the nav
// (`navFor`), and the status snippet reading one source of truth rather than
// each re-deriving the mode. A null config (empty storage or a wiped blob)
// defaults to online.
import type { PersistedConfig } from "./config.ts";

export type AppMode = "online" | "offline";

export function modeOf(config: PersistedConfig | null): AppMode {
  return config?.activeMode ?? "online";
}
