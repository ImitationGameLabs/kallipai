import { assertEquals } from "@std/assert";
import {
  initConfigStorage,
  loadConfig,
  type PersistedConfig,
} from "./config.ts";
import type { ConfigStorage } from "./storage.ts";

// A fake raw-string KV. Records saves and clearings so the wipe rule (anything
// unrecognized is cleared, empty storage is not) can be observed. `saved` is an
// array (mutated in place, safe to destructure); `cleared` is exposed as a
// getter so the assertion reads the live count rather than a snapshot taken at
// destructure time.
function fakeStorage(initial: string | null): {
  storage: ConfigStorage;
  saved: string[];
  readonly cleared: number;
} {
  let value: string | null = initial;
  const saved: string[] = [];
  let cleared = 0;
  return {
    saved,
    get cleared() {
      return cleared;
    },
    storage: {
      load: () => Promise.resolve(value),
      save: (raw: string) => {
        value = raw;
        saved.push(raw);
        return Promise.resolve();
      },
      clear: () => {
        value = null;
        cleared += 1;
        return Promise.resolve();
      },
    },
  };
}

const validOffline: PersistedConfig = {
  activeMode: "offline",
  offline: { tagmaUrl: "http://host:3000", authToken: "t" },
};

Deno.test(
  "loadConfig returns a valid persisted config unchanged and writes nothing",
  async () => {
    const fake = fakeStorage(JSON.stringify(validOffline));
    initConfigStorage(fake.storage);

    assertEquals(await loadConfig(), validOffline);
    // A recognized blob is neither rewritten nor cleared.
    assertEquals(fake.saved.length, 0);
    assertEquals(fake.cleared, 0);
  },
);

Deno.test(
  "loadConfig wipes a legacy backend-tagged shape (no activeMode)",
  async () => {
    const fake = fakeStorage(
      JSON.stringify({
        backend: "offline",
        tagmaUrl: "http://host:3000",
        authToken: "t",
      }),
    );
    initConfigStorage(fake.storage);

    assertEquals(await loadConfig(), null);
    assertEquals(fake.cleared, 1);
  },
);

Deno.test("loadConfig wipes malformed JSON", async () => {
  const fake = fakeStorage("{not json");
  initConfigStorage(fake.storage);

  assertEquals(await loadConfig(), null);
  assertEquals(fake.cleared, 1);
});

Deno.test(
  "loadConfig returns null for empty storage without clearing",
  async () => {
    const fake = fakeStorage(null);
    initConfigStorage(fake.storage);

    assertEquals(await loadConfig(), null);
    // Empty storage is the fresh-install default; no spurious write.
    assertEquals(fake.cleared, 0);
  },
);

Deno.test("loadConfig wipes a value with an unknown activeMode", async () => {
  const fake = fakeStorage(JSON.stringify({ activeMode: "spaceship" }));
  initConfigStorage(fake.storage);

  assertEquals(await loadConfig(), null);
  assertEquals(fake.cleared, 1);
});

Deno.test(
  "loadConfig wipes a stale offline shape (pre-rename daemonUrl field)",
  async () => {
    // A pre-rename blob carries `daemonUrl` instead of `tagmaUrl`; it must be
    // wiped (not loaded with an undefined URL) per the no-legacy-shapes rule.
    const fake = fakeStorage(
      JSON.stringify({
        activeMode: "offline",
        offline: { daemonUrl: "http://host:3000", authToken: "t" },
      }),
    );
    initConfigStorage(fake.storage);

    assertEquals(await loadConfig(), null);
    assertEquals(fake.cleared, 1);
  },
);

Deno.test(
  "loadConfig wipes an offline shape missing required string fields",
  async () => {
    const fake = fakeStorage(
      JSON.stringify({ activeMode: "offline", offline: { tagmaUrl: 3000 } }),
    );
    initConfigStorage(fake.storage);

    assertEquals(await loadConfig(), null);
    assertEquals(fake.cleared, 1);
  },
);
