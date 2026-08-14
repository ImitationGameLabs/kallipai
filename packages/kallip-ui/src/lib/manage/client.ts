// Shared management backend provider.
//
// In offline mode, builds an OfflineBackend from TagmaClient using
// configStore.offline (tagmaUrl + authToken).
//
// In online mode, the caller passes a RelayChannel to createOnlineBackend()
// — the management page gets the channel from the channels store for its tagma.
//
// The stores call managementBackend() for offline mode. For online mode, the
// page creates an OnlineBackend and calls store.switchBackend().

import { TagmaClient } from "@kallipai/kallip-client";
import { configStore } from "../config/config.svelte.ts";
import { OfflineBackend, type ManagementBackend } from "./backend.ts";

/**
 * Build an offline ManagementBackend from configStore.offline, or throw if no
 * offline config is set.
 */
export function managementBackend(): ManagementBackend {
  const offline = configStore.value?.offline;
  if (!offline) {
    throw new Error("management requires offline tagma configuration");
  }
  return new OfflineBackend(
    new TagmaClient({
      baseUrl: offline.tagmaUrl,
      authToken: offline.authToken,
    }),
  );
}

export type { ManagementBackend };
export { OfflineBackend, OnlineBackend } from "./backend.ts";
