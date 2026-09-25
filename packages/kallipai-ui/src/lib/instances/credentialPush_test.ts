// Pure-logic tests for the one-click credential push: wire assembly, the
// eligibility gate, loop termination states, and probe verdict mapping.
// Time and transport are injected, so no real channel or clock is touched.

import { assertEquals } from "@std/assert";
import {
  buildPushConfig,
  applyReachedAgent,
  putEchoedOurKey,
  ENROLL_PUSH_INTERVAL_MS,
  ENROLL_PUSH_WINDOW_MS,
  isLocked,
  probeRequestFor,
  probeVerdict,
  providerEndpointKey,
  pushCredentials,
  pushErrorKind,
  type PushPorts,
} from "./credentialPush.ts";
import { KallipaiError, TransportError } from "@kallipai/kallipai-common";
import type {
  ProfileConfig,
  ProfileConfigPutRequest,
  ProfileProbeResponse,
} from "@kallipai/kallipai-client";

const live: ProfileConfig = {
  sets: {
    main: {
      description: null,
      profiles: [
        {
          id: "p1",
          endpoint: "main",
          model: "deepseek-chat",
          max_context_window: 128_000,
        },
      ],
    },
  },
  endpoints: {
    main: {
      id: "main",
      family: "deepseek",
      api_key: "sk-existing",
      base_url: null,
    },
  },
  parking: [],
};

const target = {
  endpointKey: "provider:tagma-ab12cd34",
  family: "deepseek",
  apiKey: "sk-vault-secret",
  baseUrl: null,
  model: "deepseek-chat",
};

function instantPorts(
  overrides: Partial<PushPorts> = {},
): PushPorts & { puts: number; closed: boolean } {
  // Fake clock: sleeps advance it, so the deadline fold is deterministic.
  let clock = 0;
  const ports = {
    puts: 0,
    closed: false,
    fetchLive: () => Promise.resolve(live),
    put: (_body: ProfileConfigPutRequest) => {
      ports.puts++;
      // A faithful echo masks our key as head4+8stars+tail4 (mask_key).
      return Promise.resolve({
        endpoints: {
          [target.endpointKey]: {
            id: target.endpointKey,
            family: target.family,
            api_key: "sk-v********cret",
            base_url: null,
          },
        },
      });
    },
    apply: () => Promise.resolve({ applied: 1, skipped: 0 }),
    probe: () =>
      Promise.resolve({
        results: [],
        sets: [],
      }) as Promise<ProfileProbeResponse>,
    now: () => clock,
    sleep: (ms: number) => {
      clock += ms;
      return Promise.resolve();
    },
    close: () => {
      ports.closed = true;
    },
    ...overrides,
  };
  return ports as unknown as PushPorts & { puts: number; closed: boolean };
}

Deno.test("endpoint key stays within the TOML-safe charset", () => {
  const key = providerEndpointKey("tagma-ab12cd34");
  assertEquals(key, "provider:tagma-ab12cd34");
  assertEquals(/^[a-z0-9:_-]+$/.test(key), true);
});

Deno.test("eligibility: encrypted rows lock outside a passkey session", () => {
  assertEquals(isLocked({ mode: "encrypted" }, true), false);
  assertEquals(isLocked({ mode: "encrypted" }, false), true);
  // Plaintext never locks, whatever the session came through.
  assertEquals(isLocked({ mode: "plaintext" }, false), false);
});

Deno.test("wire assembly round-trips live rows and adds one endpoint", () => {
  const body = buildPushConfig(live, target);
  // The live set round-trips plus one appended binding (failover slot).
  assertEquals(body.sets.length, 1);
  assertEquals(body.sets[0].name, "main");
  assertEquals(body.sets[0].profiles[0], live.sets.main.profiles[0]);
  assertEquals(body.sets[0].profiles[1], {
    id: `profile:${target.endpointKey}`,
    endpoint: target.endpointKey,
    model: target.model,
    max_context_window: 128_000,
  });
  // The existing endpoint keeps by tri-state, ours carries the real key.
  assertEquals(body.endpoints["main"].api_key, null);
  assertEquals(body.endpoints["main"].base_url, null);
  assertEquals(body.endpoints[target.endpointKey].api_key, target.apiKey);
  assertEquals(body.endpoints[target.endpointKey].family, target.family);
  // Parking list replaces (the UI always sends it).
  assertEquals(body.parking, []);
});

Deno.test(
  "wire assembly over an empty config makes our binding the sole set",
  () => {
    const body = buildPushConfig(
      { sets: {}, endpoints: {}, parking: [] },
      target,
    );
    assertEquals(Object.keys(body.endpoints), [target.endpointKey]);
    assertEquals(body.sets.length, 1);
    assertEquals(body.sets[0].name, "default");
    assertEquals(body.sets[0].profiles, [
      {
        id: `profile:${target.endpointKey}`,
        endpoint: target.endpointKey,
        model: target.model,
        max_context_window: 128_000,
      },
    ]);
  },
);

Deno.test("re-pushing the same instance overwrites its entry in place", () => {
  // The live config as GET would return it: our binding already inside.
  const liveOnce: ProfileConfig = {
    sets: {
      main: {
        description: null,
        profiles: [
          ...live.sets.main.profiles,
          {
            id: `profile:${target.endpointKey}`,
            endpoint: target.endpointKey,
            model: target.model,
            max_context_window: 128_000,
          },
        ],
      },
    },
    endpoints: {
      main: live.endpoints.main,
      [target.endpointKey]: {
        id: target.endpointKey,
        family: target.family,
        api_key: "sk-old",
        base_url: null,
      },
    },
    parking: [],
  };
  const twice = buildPushConfig(liveOnce, { ...target, apiKey: "sk-rotated" });
  assertEquals(twice.endpoints[target.endpointKey].api_key, "sk-rotated");
  assertEquals(Object.keys(twice.endpoints).length, 2);
  // The re-push replaces the prior binding too (no duplicate profile id).
  const bindings = twice.sets[0].profiles.filter(
    (p) => p.endpoint === target.endpointKey,
  );
  assertEquals(bindings.length, 1);
});

Deno.test(
  "probe request references our endpoint inline with keep-nulls",
  () => {
    const req = probeRequestFor(target);
    assertEquals(req.endpoints.length, 1);
    assertEquals(req.endpoints[0], {
      id: target.endpointKey,
      family: target.family,
      api_key: null,
      base_url: target.baseUrl,
    });
    assertEquals(req.sets.length, 0);
  },
);

Deno.test("probe verdict maps only our endpoint's ok to true", () => {
  const ok = {
    results: [{ endpoint_id: target.endpointKey, status: "ok" }],
    sets: [],
  } as unknown as ProfileProbeResponse;
  const bad = {
    results: [{ endpoint_id: target.endpointKey, status: "unauthorized" }],
    sets: [],
  } as unknown as ProfileProbeResponse;
  const other = {
    results: [{ endpoint_id: "elsewhere", status: "ok" }],
    sets: [],
  } as unknown as ProfileProbeResponse;
  assertEquals(probeVerdict(ok, target.endpointKey), true);
  assertEquals(probeVerdict(bad, target.endpointKey), false);
  assertEquals(probeVerdict(other, target.endpointKey), false);
});

Deno.test("mask echo: our key's tail + stars reads as stored", () => {
  const body = buildPushConfig(live, target);
  // The PUT response is the GET shape: sets keyed by name, keys masked.
  const masked: ProfileConfig = {
    sets: Object.fromEntries(
      body.sets.map((s) => [
        s.name,
        { description: s.description, profiles: s.profiles },
      ]),
    ),
    endpoints: {
      ...body.endpoints,
      [target.endpointKey]: {
        ...body.endpoints[target.endpointKey],
        api_key: "sk-v********cret",
      },
    },
    parking: body.parking,
  };
  assertEquals(
    putEchoedOurKey(masked, target.endpointKey, target.apiKey),
    true,
  );
  // A null keep means our endpoint never landed: not stored.
  const nullKeep: ProfileConfig = {
    ...masked,
    endpoints: {
      ...masked.endpoints,
      [target.endpointKey]: {
        ...masked.endpoints[target.endpointKey],
        api_key: null,
      },
    },
  };
  assertEquals(
    putEchoedOurKey(nullKeep, target.endpointKey, target.apiKey),
    false,
  );
  // An unrelated string (wrong tail) is not our key either.
  const other = {
    ...masked,
    endpoints: {
      ...masked.endpoints,
      [target.endpointKey]: {
        ...masked.endpoints[target.endpointKey],
        api_key: "zz-z********zz-z",
      },
    },
  };
  assertEquals(
    putEchoedOurKey(other, target.endpointKey, target.apiKey),
    false,
  );
  // A short key (<=8 chars) masks as eight bare stars: still stored.
  assertEquals(
    putEchoedOurKey(
      {
        ...masked,
        endpoints: {
          ...masked.endpoints,
          [target.endpointKey]: {
            ...masked.endpoints[target.endpointKey],
            api_key: "********",
          },
        },
      },
      target.endpointKey,
      "shortkey",
    ),
    true,
  );
});

Deno.test("apply gate: applied>=1 is the delivery bar", () => {
  assertEquals(applyReachedAgent({ applied: 1, skipped: 0 }), true);
  assertEquals(applyReachedAgent({ applied: 0, skipped: 2 }), false);
});

Deno.test("error classification: transient vs terminal", () => {
  assertEquals(pushErrorKind(new TransportError("ws died")), "retry");
  assertEquals(
    pushErrorKind(new KallipaiError({ status: 500, message: "boom" })),
    "retry",
  );
  assertEquals(
    pushErrorKind(new KallipaiError({ status: 429, message: "slow down" })),
    "retry",
  );
  assertEquals(pushErrorKind(new Error("crypto failed")), "retry");
  // A structured rejection (bad key shape) cannot succeed on retry.
  assertEquals(
    pushErrorKind(new KallipaiError({ status: 400, message: "bad api_key" })),
    "terminal",
  );
  assertEquals(
    pushErrorKind(new KallipaiError({ status: 401, message: "denied" })),
    "terminal",
  );
});

Deno.test(
  "push loop: first-attempt success reports pushed + probe verdict",
  async () => {
    const ports = instantPorts({
      probe: () =>
        Promise.resolve({
          results: [{ endpoint_id: target.endpointKey, status: "ok" }],
          sets: [],
        }) as unknown as Promise<ProfileProbeResponse>,
    });
    const outcome = await pushCredentials(target, ports);
    assertEquals(outcome.state, "pushed");
    if (outcome.state === "pushed") assertEquals(outcome.probeOk, true);
    assertEquals(ports.puts, 1);
    assertEquals(ports.closed, true);
  },
);

Deno.test(
  "push loop: terminal 400 stops immediately with the message",
  async () => {
    let attempts = 0;
    const ports = instantPorts({
      put: () => {
        attempts++;
        return Promise.reject(
          new KallipaiError({
            status: 400,
            message: "api_key must not be empty",
          }),
        );
      },
    });
    const outcome = await pushCredentials(target, ports);
    assertEquals(outcome, {
      state: "failed",
      endpointKey: target.endpointKey,
      message: "api_key must not be empty",
    });
    assertEquals(attempts, 1);
    assertEquals(ports.closed, true);
  },
);

Deno.test(
  "push loop: transient failures retry until the window folds",
  async () => {
    // Fold check fires after the attempt whose next sleep would cross the
    // deadline, so total attempts = ceil(WINDOW / INTERVAL).
    const expectedAttempts = Math.ceil(
      ENROLL_PUSH_WINDOW_MS / ENROLL_PUSH_INTERVAL_MS,
    );
    let attempts = 0;
    const ports = instantPorts({
      put: () => {
        attempts++;
        return Promise.reject(new TransportError("not enrolled yet"));
      },
    });
    const outcome = await pushCredentials(target, ports);
    assertEquals(attempts, expectedAttempts);
    assertEquals(outcome.state, "unreachable");
    assertEquals(ports.closed, true);
  },
);

Deno.test("push loop: recovery inside the window still pushes", async () => {
  let attempts = 0;
  const ports = instantPorts({
    put: () => {
      attempts++;
      if (attempts < 3) return Promise.reject(new TransportError("soon"));
      // Faithful masked echo (see the shared stub above).
      return Promise.resolve({
        endpoints: {
          [target.endpointKey]: {
            id: target.endpointKey,
            family: target.family,
            api_key: "sk-v********cret",
            base_url: null,
          },
        },
      });
    },
  });
  const outcome = await pushCredentials(target, ports);
  assertEquals(outcome.state, "pushed");
  assertEquals(attempts, 3);
});

Deno.test(
  "push loop: a probe failure after a good PUT degrades probeOk only",
  async () => {
    const ports = instantPorts({
      probe: () => Promise.reject(new TransportError("probe dropped")),
    });
    const outcome = await pushCredentials(target, ports);
    assertEquals(outcome.state, "pushed");
    if (outcome.state === "pushed") assertEquals(outcome.probeOk, false);
  },
);
