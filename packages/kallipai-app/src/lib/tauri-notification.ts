// The tauri notification backend: maps kallipai-ui's NotificationBackend port
// onto @tauri-apps/plugin-notification. Injected once at app bootstrap
// (initNotificationBackend in the root layout), so the shared notify() choke
// point routes through the plugin in the app shell while web shells keep the
// Web Notification default. Single slot = a shell can never send through both
// faces.
//
// tag -> `group`: the plugin has no WHATWG tag option; `group` is the
// platform thread/group identifier (iOS threadIdentifier, Android group), so
// a conversation's burst at least threads together. It is NOT a strict
// same-tag replacement -- the asymmetry is inherent to platform grouping
// and still needs on-device verification.
//
// permission(): the plugin only answers a boolean, which cannot distinguish
// "never asked" from "refused". The last requestPermission() answer is
// cached to keep the tri-state honest: granted wins outright; otherwise the
// cached refusal (if any) or the ask-me default.
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";
import type {
  NotificationBackend,
  PermissionState,
} from "@kallipai/kallipai-ui";

let lastRequested: PermissionState | null = null;

export const tauriNotificationBackend: NotificationBackend = {
  async permission(): Promise<PermissionState> {
    if (await isPermissionGranted()) return "granted";
    return lastRequested ?? "default";
  },
  async requestPermission(): Promise<PermissionState> {
    // Wraps the plugin's Android 13+ POST_NOTIFICATIONS runtime request; the
    // desktop/WebView branches resolve through their own permission stores.
    const answer = await requestPermission();
    lastRequested = answer;
    return answer;
  },
  show({ title, body, tag }) {
    sendNotification({ title, body, group: tag });
  },
};
