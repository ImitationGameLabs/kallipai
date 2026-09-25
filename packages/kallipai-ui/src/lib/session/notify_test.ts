// Tests for the notification send-face choke point: the guard chain
// (window visibility -> user switch -> permission) silently suppresses, the
// single backend slot routes show() to exactly one face (the structural
// double-send impossibility), and the rooms floor decision is the pure
// watermark truth table. Rune-bearing module under `deno test`: passthrough
// $state shim (the unread_test pattern). No DOM: the default web backend's
// missing-Notification silence is itself asserted.
declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const {
  initNotificationBackend,
  notify,
  notificationPermission,
  requestNotificationPermission,
  setHiddenProbe,
  shouldNotifyRoom,
} = await import("./notify.ts");
type PermissionState = "granted" | "denied" | "default";
const { configStore } = await import("../config/config.svelte.ts");

// -- fake backend ------------------------------------------------------------

function fakeBackend(
  permission: "granted" | "denied" | "default" = "granted",
): {
  backend: Parameters<typeof initNotificationBackend>[0];
  shown: { tag: string; title: string; body: string }[];
  permissions: PermissionState[];
} {
  const shown: { tag: string; title: string; body: string }[] = [];
  const permissions: PermissionState[] = [];
  return {
    permissions,
    shown,
    backend: {
      permission: () => {
        permissions.push(permission);
        return Promise.resolve(permission);
      },
      requestPermission: () => {
        permissions.push(permission);
        return Promise.resolve(permission);
      },
      show: (n) => shown.push(n),
    },
  };
}

function resetSeams(): void {
  initNotificationBackend(null);
  setHiddenProbe(null);
  configStore.value = null;
}

// -- the guard chain ---------------------------------------------------------

Deno.test("notify stays silent while the window is visible", async () => {
  const fake = fakeBackend("granted");
  initNotificationBackend(fake.backend);
  setHiddenProbe(() => false); // foreground: the transcript IS the delivery
  configStore.value = { activeMode: "online", notificationsEnabled: true };

  await notify({ tag: "tagma:t1", title: "T", body: "B" });

  assertEquals(fake.shown.length, 0);
  // The permission is never even consulted behind the visibility guard.
  assertEquals(fake.permissions.length, 0);
  resetSeams();
});

Deno.test(
  "notify stays silent while the user switch is off or absent",
  async () => {
    const fake = fakeBackend("granted");
    initNotificationBackend(fake.backend);
    setHiddenProbe(() => true);

    // The opt-in default: no config, or the field missing, both mean off.
    configStore.value = null;
    await notify({ tag: "tagma:t1", title: "T", body: "B" });
    assertEquals(fake.shown.length, 0);

    configStore.value = { activeMode: "online", notificationsEnabled: false };
    await notify({ tag: "tagma:t1", title: "T", body: "B" });
    assertEquals(fake.shown.length, 0);
    resetSeams();
  },
);

Deno.test("notify stays silent unless permission is granted", async () => {
  setHiddenProbe(() => true);
  configStore.value = { activeMode: "online", notificationsEnabled: true };

  for (const state of ["denied", "default"] as const) {
    const fake = fakeBackend(state);
    initNotificationBackend(fake.backend);
    await notify({ tag: "tagma:t1", title: "T", body: "B" });
    assertEquals(fake.shown.length, 0, `permission=${state} must suppress`);
    initNotificationBackend(null);
  }
  resetSeams();
});

Deno.test("notify routes one show through the injected backend", async () => {
  const fake = fakeBackend("granted");
  initNotificationBackend(fake.backend);
  setHiddenProbe(() => true);
  configStore.value = { activeMode: "online", notificationsEnabled: true };

  await notify({ tag: "room:r1", title: "room", body: "hello" });

  assertEquals(fake.shown, [{ tag: "room:r1", title: "room", body: "hello" }]);
  resetSeams();
});

Deno.test(
  "the default web backend is silent without a Notification global",
  async () => {
    // No injection: the web default. Deno has no `Notification`, which is the
    // browser shape the old maybeNotifyBackground guarded with typeof.
    setHiddenProbe(() => true);
    configStore.value = { activeMode: "online", notificationsEnabled: true };

    await notify({ tag: "tagma:t1", title: "T", body: "B" }); // must not throw
    assertEquals(await notificationPermission(), "default");
    resetSeams();
  },
);

Deno.test("the slot is single: re-injection replaces the route", async () => {
  // The double-send impossibility, structural: initNotificationBackend
  // swaps the ONLY slot, so the superseded backend can never see a show.
  const first = fakeBackend("granted");
  const second = fakeBackend("granted");
  initNotificationBackend(first.backend);
  setHiddenProbe(() => true);
  configStore.value = { activeMode: "online", notificationsEnabled: true };

  await notify({ tag: "tagma:t1", title: "T", body: "one" });
  initNotificationBackend(second.backend);
  await notify({ tag: "tagma:t1", title: "T", body: "two" });

  assertEquals(
    first.shown.map((n) => n.body),
    ["one"],
  );
  assertEquals(
    second.shown.map((n) => n.body),
    ["two"],
  );
  resetSeams();
});

Deno.test("permission queries route to the injected backend", async () => {
  const fake = fakeBackend("denied");
  initNotificationBackend(fake.backend);

  assertEquals(await notificationPermission(), "denied");
  assertEquals(await requestNotificationPermission(), "denied");
  resetSeams();
});

// -- the rooms floor decision (pure truth table) -----------------------------

Deno.test(
  "shouldNotifyRoom: own echo, viewing, or zero count all suppress",
  () => {
    // The floor is the unread watermark count. The zero-count row is
    // the envelope-before-pull race: suppressed, not faked (conservative).
    assertEquals(
      shouldNotifyRoom({ unreadCount: 3, viewing: false, own: false }),
      true,
    );
    assertEquals(
      shouldNotifyRoom({ unreadCount: 3, viewing: true, own: false }),
      false,
    );
    assertEquals(
      shouldNotifyRoom({ unreadCount: 3, viewing: false, own: true }),
      false,
    );
    assertEquals(
      shouldNotifyRoom({ unreadCount: 0, viewing: false, own: false }),
      false,
    );
  },
);
