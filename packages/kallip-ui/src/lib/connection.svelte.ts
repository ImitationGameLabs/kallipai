// Headless connection view-model. Pure projection from a session source into
// the bits the chrome (header status pill, composer disabled-state) needs. Kept
// free of any app/session import so it stays reusable across consuming apps; the
// caller passes a reactive source and wraps the call in `$derived` to retain
// reactivity.

export type ConnectionState = "connected" | "connecting" | "offline";

export interface ConnectionViewModel {
  readonly state: ConnectionState;
  readonly label: string;
  /** Skeleton background token for the status dot. */
  readonly dotClass: string;
}

/** Reactive fields the projection reads. Implement on the app session store. */
export interface ConnectionSource {
  readonly connected: boolean;
  readonly connecting: boolean;
}

export function connectionViewModel(
  src: ConnectionSource,
): ConnectionViewModel {
  if (src.connecting) {
    return {
      state: "connecting",
      label: "connecting",
      dotClass: "bg-warning-500",
    };
  }
  if (src.connected) {
    return {
      state: "connected",
      label: "connected",
      dotClass: "bg-success-500",
    };
  }
  return { state: "offline", label: "not connected", dotClass: "bg-error-500" };
}
