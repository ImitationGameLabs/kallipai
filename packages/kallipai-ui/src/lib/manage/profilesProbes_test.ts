// The Profiles page's probe call sites: the dialog entry points build from the
// draft they are handed, the card paths read the committed draft, and every
// reply lands under the key its card reads. The drafts differ on purpose: a
// reader that reached for the store's draft would probe the old model.
//
// The module's store slice is typed from the rune-bearing store module,
// which deno test runs uncompiled: a passthrough $state shim with a
// snapshot member lets both load and run with plain fields (the
// profiles-store_test pattern).

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
  namespace $state {
    function snapshot<T>(v: T): T;
  }
}

(globalThis as Record<string, unknown>)["$state"] = Object.assign(
  (v: unknown) => v,
  {
    snapshot: (v: unknown) => v,
  },
);

const { assertEquals } = await import("@std/assert");
const { createProfilesProbes } = await import("./profilesProbes.svelte.ts");
import type { ProfilesProbeStore } from "./profilesProbes.svelte.ts";
import type {
  ProfileConfig,
  ProfileProbeRequest,
  ProfileProbeResponse,
} from "@kallipai/kallipai-client";

function profile(model: string, endpoint = "openai"): Record<string, unknown> {
  return { id: `p-${model}`, endpoint, model, max_context_window: 1000 };
}

/** One provider, one set holding `model`, one parked profile. */
function config(model: string): ProfileConfig {
  return {
    endpoints: {
      openai: {
        id: "openai",
        family: "openai-compatible",
        base_url: "http://live",
        model_list_base_url: "",
        api_key: "stored",
      },
    },
    sets: { s1: { description: null, profiles: [profile(model)] } },
    parking: [profile(`parked-${model}`)],
    default: "s1",
  } as unknown as ProfileConfig;
}

/** A reply naming one provider and one profile of `s1`. */
function reply(profileId: string): ProfileProbeResponse {
  return {
    results: [{ endpoint_id: "openai", status: "ok" }],
    sets: [{ name: "s1", profiles: [{ profile_id: profileId, status: "ok" }] }],
  } as unknown as ProfileProbeResponse;
}

/** A store whose committed draft is `committed` and whose probes answer. */
function harness(committed: ProfileConfig, answer: ProfileProbeResponse) {
  const sent: ProfileProbeRequest[] = [];
  const answered: ("landed" | "failed")[] = [];
  const store = {
    config: committed,
    draft: committed,
    probeProvider: () =>
      Promise.resolve({ status: "landed", response: answer }),
    probeSet: () => Promise.resolve({ status: "landed", response: answer }),
    probeAll: () => Promise.resolve({ status: "landed", response: answer }),
    probeRaw: (body: ProfileProbeRequest) => {
      sent.push(body);
      return Promise.resolve({
        status: answered[0] ?? "landed",
        response: answer,
      });
    },
  } as unknown as ProfilesProbeStore;
  return {
    probes: createProfilesProbes({ store }),
    sent,
    setAnswer: (status: "landed" | "failed") => {
      answered[0] = status;
    },
  };
}

// --- dialog entry points read the draft they are handed ---

Deno.test(
  "probeProfileAt: the request carries the handed draft's model",
  async () => {
    const committed = config("committed");
    const h = harness(committed, reply("p-typed"));
    const staged = config("typed");
    const outcome = await h.probes.probeProfileAt("s1", 0, staged);
    assertEquals(h.sent.length, 1);
    assertEquals(h.sent[0].sets[0].profiles[0].model, "typed");
    assertEquals(outcome?.status, "landed");
  },
);

Deno.test(
  "probeProfileAt: an unknown set sends nothing and returns null",
  async () => {
    const h = harness(config("m"), reply("p-m"));
    const outcome = await h.probes.probeProfileAt("gone", 0, config("m"));
    assertEquals(outcome, null);
    assertEquals(h.sent, []);
  },
);

Deno.test(
  "probeUnassignedAt: the request carries the handed draft's parked model",
  async () => {
    const h = harness(config("m"), reply("p-typed"));
    const outcome = await h.probes.probeUnassignedAt(0, config("typed"));
    assertEquals(h.sent.length, 1);
    assertEquals(h.sent[0].sets[0].profiles[0].model, "parked-typed");
    assertEquals(outcome?.status, "landed");
  },
);

Deno.test(
  "probeUnassignedAt: an index past the end sends nothing and returns null",
  async () => {
    const h = harness(config("m"), reply("p-m"));
    const outcome = await h.probes.probeUnassignedAt(9, config("m"));
    assertEquals(outcome, null);
    assertEquals(h.sent, []);
  },
);

// --- card paths read the committed draft and key their replies ---

Deno.test(
  "cardTestProfile: both maps take the reply, under their own keys",
  async () => {
    const h = harness(config("m"), reply("p-m"));
    await h.probes.cardTestProfile("s1", 0);
    assertEquals([...h.probes.providerReports.keys()], ["openai"]);
    assertEquals([...h.probes.profileReports.keys()], ["s1:p-m"]);
  },
);

Deno.test(
  "cardTestProfile: a failed probe leaves both maps untouched",
  async () => {
    const h = harness(config("m"), reply("p-m"));
    h.setAnswer("failed");
    await h.probes.cardTestProfile("s1", 0);
    assertEquals(h.probes.providerReports.size, 0);
    assertEquals(h.probes.profileReports.size, 0);
  },
);

Deno.test(
  "cardTestUnassigned: the report lands under the parked id, not the reply's",
  async () => {
    const h = harness(config("m"), reply("some-other-id"));
    await h.probes.cardTestUnassigned(0);
    assertEquals([...h.probes.profileReports.keys()], ["p:p-parked-m"]);
  },
);

Deno.test("cardTestAll: each reply set keys off its own name", async () => {
  const all = {
    results: [{ endpoint_id: "openai", status: "ok" }],
    sets: [
      { name: "s1", profiles: [{ profile_id: "a", status: "ok" }] },
      { name: "s2", profiles: [{ profile_id: "b", status: "ok" }] },
    ],
  } as unknown as ProfileProbeResponse;
  const h = harness(config("m"), all);
  await h.probes.cardTestAll();
  assertEquals([...h.probes.profileReports.keys()].sort(), ["s1:a", "s2:b"]);
});

// --- clearing ---

Deno.test(
  "clearSetProfile and clearUnassignedProfile drop only their key",
  async () => {
    const h = harness(config("m"), reply("p-m"));
    await h.probes.cardTestProfile("s1", 0);
    await h.probes.cardTestUnassigned(0);
    assertEquals(h.probes.profileReports.size, 2);
    h.probes.clearSetProfile("s1", "p-m");
    assertEquals([...h.probes.profileReports.keys()], ["p:p-parked-m"]);
    h.probes.clearUnassignedProfile("p-parked-m");
    assertEquals(h.probes.profileReports.size, 0);
    assertEquals(h.probes.providerReports.size, 1);
  },
);

Deno.test("clearProfileCards keeps the provider cards", async () => {
  const h = harness(config("m"), reply("p-m"));
  await h.probes.cardTestProfile("s1", 0);
  await h.probes.cardTestUnassigned(0);
  h.probes.clearProfileCards();
  assertEquals(h.probes.profileReports.size, 0);
  assertEquals(h.probes.providerReports.size, 1);
});

Deno.test("clearAll empties both maps", async () => {
  const h = harness(config("m"), reply("p-m"));
  await h.probes.cardTestProfile("s1", 0);
  h.probes.clearAll();
  assertEquals(h.probes.providerReports.size, 0);
  assertEquals(h.probes.profileReports.size, 0);
});
