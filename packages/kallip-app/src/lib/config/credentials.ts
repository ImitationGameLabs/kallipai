// Single facade for persisted connection config. This is the only module that
// touches storage, so the phase-2 swap to Tauri secure storage is a one-file
// change. (Phase 1 uses localStorage; tokens are XSS-exposed, acceptable for a
// local-first desktop context.)

const KEY = "kallip:connection";

export interface DirectConfig {
  readonly backend: "direct";
  readonly daemonUrl: string;
  readonly authToken: string;
}

export interface AgoraConfig {
  readonly backend: "agora";
  readonly agoraUrl: string;
  readonly userToken: string;
  readonly teamId: string;
  readonly agentId: string;
  readonly pinnedPublicKey?: string;
}

export type ConnectionConfig = DirectConfig | AgoraConfig;

export function loadConfig(): ConnectionConfig | null {
  const raw = localStorage.getItem(KEY);
  if (!raw) return null;
  try {
    return JSON.parse(raw) as ConnectionConfig;
  } catch {
    return null;
  }
}

export function saveConfig(config: ConnectionConfig): void {
  localStorage.setItem(KEY, JSON.stringify(config));
}

export function clearConfig(): void {
  localStorage.removeItem(KEY);
}
