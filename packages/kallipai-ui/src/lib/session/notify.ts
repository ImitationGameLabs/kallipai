// The notification send-face. Every system
// notification funnels through {@link notify}: one choke point that owns the
// guard chain, with the platform specifics behind a single-slot backend.
//
//   - Default backend: the Web Notification API (what a browser shell uses).
//   - App shell: the host injects the tauri notification plugin backend at
//     bootstrap (initNotificationBackend in kallipai-app's root layout). The
//     slot holds exactly ONE backend, so a shell can never send through both
//     faces -- the double-send impossibility is structural, not
//     discipline.
//
// Guard chain, all silent-return (an ineligible notification is
// never an error):
//   1. Window visibility. A visible window never notifies -- the transcript
//      IS the delivery. Hidden-window semantics live HERE: the unread store
//      keys "viewing" on the mounted conversation page (per-conversation),
//      this layer owns the app-level document visibility signal.
//   2. The user's settings switch (persisted notificationsEnabled; default
//      off -- notifications are opt-in).
//   3. The platform permission (granted only).
//
// Tag merge (the researcher fold-in): the tag is the conversation key
// (`tagma:{id}` / `room:{id}` -- the unread store's key space, so a
// conversation's burst collapses onto one notification). The web backend
// passes it as the WHATWG `tag` (same-origin same-tag replacement). The
// tauri plugin has no WHATWG tag; its backend maps tag -> `group` (the
// platform thread/group identifier) -- threads the burst, but is NOT a
// strict replacement; the asymmetry is inherent to platform grouping.

import { configStore } from "../config/config.svelte.ts";

/** The platform permission tri-state (the Web Notifications vocabulary;
 *  the tauri plugin's requestPermission speaks the same states). */
export type PermissionState = "granted" | "denied" | "default";

/** The platform face behind the choke point. One method set, three
 *  responsibilities: report the permission, ask for it (the settings
 *  toggle's click is the user gesture), and show one notification. */
export interface NotificationBackend {
  permission(): Promise<PermissionState>;
  requestPermission(): Promise<PermissionState>;
  show(notification: { tag: string; title: string; body: string }): void;
}

// The browser globals are reached through a `globalThis` feature probe, not
// bare identifiers: the package type-checks under both DOM-lib environments
// (svelte-check) and Deno's test lib, where `Notification`/`document` do not
// exist as types. Runtime behavior is identical (undefined = absent).
type NotificationApi = {
  readonly permission: PermissionState;
  requestPermission(): Promise<PermissionState>;
  new (title: string, options?: { tag?: string; body?: string }): unknown;
};

function browserApi(): NotificationApi | undefined {
  return (globalThis as { Notification?: NotificationApi }).Notification;
}

function documentHidden(): boolean {
  return (
    (globalThis as { document?: { hidden: boolean } }).document?.hidden ?? false
  );
}

/** The Web Notification API backend. Every method tolerates a missing
 *  `Notification` global (non-browser runtimes, the Deno test environment):
 *  permission reads as the ask-me state and show is a no-op, mirroring the
 *  old maybeNotifyBackground's undefined-API silence. */
const webNotificationBackend: NotificationBackend = {
  permission() {
    const api = browserApi();
    return Promise.resolve(api === undefined ? "default" : api.permission);
  },
  requestPermission() {
    const api = browserApi();
    if (api === undefined) return Promise.resolve("default");
    return api.requestPermission();
  },
  show(notification) {
    const api = browserApi();
    if (api === undefined) return;
    try {
      // The WHATWG tag replacement: a same-tag notification replaces the
      // visible one -- a conversation's burst stays one notification.
      new api(notification.title, {
        tag: notification.tag,
        body: notification.body,
      });
    } catch {
      // Construction without a service worker rejects on some browsers; a
      // failed notification must never break the message path.
    }
  },
};

let backend: NotificationBackend = webNotificationBackend;

function defaultHiddenProbe(): boolean {
  return documentHidden();
}

let hiddenProbe: () => boolean = defaultHiddenProbe;

/** Inject the platform backend (kallipai-app's bootstrap passes the tauri
 *  plugin backend; null restores the Web Notification default). Idempotent;
 *  the slot is the double-send impossibility. */
export function initNotificationBackend(b: NotificationBackend | null): void {
  backend = b ?? webNotificationBackend;
}

/** Test seam: override the visibility probe (null restores the real
 *  document.hidden read). */
export function setHiddenProbe(p: (() => boolean) | null): void {
  hiddenProbe = p ?? defaultHiddenProbe;
}

/** The current permission state (the settings section reads this to render
 *  the denied guidance proactively, not only after a failed request). */
export function notificationPermission(): Promise<PermissionState> {
  return backend.permission();
}

/** Ask for permission. MUST be called from a user gesture (the settings
 *  toggle's handler is the gesture; browsers drop prompt-less requests). */
export function requestNotificationPermission(): Promise<PermissionState> {
  return backend.requestPermission();
}

/** The choke point. Guards in order (visibility, user switch, permission),
 *  all silent-return, then one backend show. Never throws: a notification
 *  failure must not disturb the message path that triggered it. */
export async function notify(notification: {
  tag: string;
  title: string;
  body: string;
}): Promise<void> {
  if (!hiddenProbe()) return;
  // The switch is opt-in: anything but an explicit true (a stale blob
  // missing the field, no config at all) means off.
  if (configStore.value?.notificationsEnabled !== true) return;
  try {
    if ((await backend.permission()) !== "granted") return;
    backend.show(notification);
  } catch {
    // A misbehaving backend is swallowed: notifications are best-effort.
  }
}

/** The rooms floor decision (floor = unread watermark comparison).
 *  Pure so the truth table is unit-testable without stores or DOM:
 *  a notification fires only for a room that is not being viewed, a line
 *  that is not my own echo, and a NON-ZERO unread count. The count test is
 *  conservative in the envelope-before-pull race (the store has not counted
 *  the first message yet -> suppressed, not faked) -- the conservative
 *  seed-0 direction: a rare missed notification beats a wrong one. */
export function shouldNotifyRoom(args: {
  unreadCount: number;
  viewing: boolean;
  own: boolean;
}): boolean {
  return !args.own && !args.viewing && args.unreadCount > 0;
}
