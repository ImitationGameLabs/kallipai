// The Profiles page's dialog controller: what each family does to the draft,
// which probe calls it forwards, and what the group surface reports. Each test
// goes red if the behavior it names changes.
//
// The aggregate's store slice is typed from the rune-bearing store module,
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
const { createProfilesDialogs } = await import("./profilesDialogs.svelte.ts");
const { KallipaiError } = await import("@kallipai/kallipai-common");
import type {
  ProfilesDialogBackend,
  ProfilesDialogStore,
  ProfilesDialogsOptions,
  ProfileFormResult,
  SetFormResult,
} from "./profilesDialogs.svelte.ts";
import type { ProfilesProbeCalls } from "./profilesProbes.svelte.ts";
import type {
  ProfileConfig,
  ProfileProbeResponse,
  ProfileProvider,
} from "@kallipai/kallipai-client";

/** A draft: one provider, one set of `models`, nothing unassigned. */
function draftWith(
  setName: string,
  models: string[],
  parking: string[] = [],
): ProfileConfig {
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
    sets: {
      [setName]: {
        description: "old",
        profiles: models.map((id) => ({
          id,
          endpoint: "openai",
          model: id,
          max_context_window: 1000,
        })),
      },
    },
    parking: parking.map((id) => ({
      id,
      endpoint: "openai",
      model: id,
      max_context_window: 1000,
    })),
    default: setName,
  } as unknown as ProfileConfig;
}

/** A landed probe reply: one set with one profile. */
function reply(setName: string, id: string): ProfileProbeResponse {
  return {
    sets: [{ name: setName, profiles: [{ profile_id: id, status: "ok" }] }],
    results: [],
  } as unknown as ProfileProbeResponse;
}

/** One profile as ProfileDialog submits it. */
function form(id: string, model: string): ProfileFormResult {
  return {
    id,
    endpoint: "openai",
    model,
    max_context_window: 1000,
  };
}

function setForm(
  name: string,
  description: string | null,
  models: string[],
): SetFormResult {
  return {
    name,
    description,
    rows: models.map((id) => ({
      id,
      endpoint: "openai",
      model: id,
      max_context_window: 1000,
    })),
  };
}

/** The controller wired to fakes that record every call it makes. */
function harness(initial: ProfileConfig | null) {
  const calls = {
    resetProbe: 0,
    merged: 0,
    clearedProfileCards: 0,
    removedProviders: [] as string[],
    removedSets: [] as string[],
    clearedProviders: [] as string[],
    clearedSetProfiles: [] as string[],
    clearedUnassigned: [] as string[],
    deletes: [] as [string, boolean][],
    refreshed: 0,
    probeDrafts: [] as {
      setName: string | null;
      idx: number;
      draft: ProfileConfig;
    }[],
  };
  const state = { draft: initial };
  const store = {
    get draft() {
      return state.draft;
    },
    set draft(next: ProfileConfig | null) {
      state.draft = next;
    },
    resetProbe: () => {
      calls.resetProbe += 1;
    },
    removeProvider: (id: string) => {
      calls.removedProviders.push(id);
    },
    removeSet: (name: string) => {
      calls.removedSets.push(name);
    },
  } as unknown as ProfilesDialogStore;

  let deleteError: unknown = null;
  const backend = {
    listAgents: () =>
      Promise.resolve({
        agents: [
          { id: "a1", role: "dev", profile_set: "s1" },
          { id: "a2", role: "", profile_set: "other" },
        ],
      }),
    deleteProfileSet: (name: string, force: boolean) => {
      calls.deletes.push([name, force]);
      return deleteError === null
        ? Promise.resolve({ ok: true })
        : Promise.reject(deleteError);
    },
  } as unknown as ProfilesDialogBackend;

  const probes = {
    clearProfileCards: () => {
      calls.clearedProfileCards += 1;
    },
    clearProvider: (id: string) => {
      calls.clearedProviders.push(id);
    },
    clearSetProfile: (setName: string, id: string) => {
      calls.clearedSetProfiles.push(`${setName}:${id}`);
    },
    clearUnassignedProfile: (id: string) => {
      calls.clearedUnassigned.push(id);
    },
    probeProfileAt: (setName: string, idx: number, draft: ProfileConfig) => {
      calls.probeDrafts.push({ setName, idx, draft });
      return Promise.resolve({
        status: "landed",
        response: reply(setName, "probed"),
      });
    },
    probeUnassignedAt: (idx: number, draft: ProfileConfig) => {
      calls.probeDrafts.push({ setName: null, idx, draft });
      return Promise.resolve({
        status: "landed",
        response: reply("x", "probed"),
      });
    },
    mergeProviders: () => {
      calls.merged += 1;
    },
  } as unknown as ProfilesProbeCalls;

  const options: ProfilesDialogsOptions = {
    store,
    backend: () => backend,
    probes,
    refreshUnassignedLive: () => {
      calls.refreshed += 1;
      return Promise.resolve();
    },
  };
  return {
    dialogs: createProfilesDialogs(options),
    store,
    calls,
    draft: () => state.draft,
    failDelete: (e: unknown) => {
      deleteError = e;
    },
  };
}

// --- set form ---

Deno.test(
  "setSave: the rows land under the new name (rename runs first)",
  () => {
    const h = harness(draftWith("old", ["m1", "m2"]));
    h.dialogs.set.show("old");
    h.dialogs.set.save(setForm("new", "updated", ["m1x"]));
    const draft = h.draft()!;
    assertEquals(draft.sets.old, undefined);
    assertEquals(
      draft.sets.new.profiles.map((p) => p.model),
      ["m1x"],
    );
    assertEquals(draft.sets.new.description, "updated");
    assertEquals(draft.default, "new");
    assertEquals(h.dialogs.set.open, false);
  },
);

Deno.test(
  "setSave: a rename clears the profile cards, an unchanged name keeps them",
  () => {
    const renamed = harness(draftWith("old", ["m1"]));
    renamed.dialogs.set.show("old");
    renamed.dialogs.set.save(setForm("new", null, ["m1"]));
    assertEquals(renamed.calls.clearedProfileCards, 1);
    assertEquals(renamed.calls.clearedProviders, []);

    const kept = harness(draftWith("old", ["m1"]));
    kept.dialogs.set.show("old");
    kept.dialogs.set.save(setForm("old", "edited", ["m1"]));
    assertEquals(kept.calls.clearedProfileCards, 0);
    assertEquals(kept.calls.clearedProviders, []);
  },
);

// --- set remove ---

Deno.test(
  "setRemoveAsk: lists the agents bound to that set by role",
  async () => {
    const h = harness(draftWith("s1", ["m1"]));
    await h.dialogs.setRemove.ask("s1");
    assertEquals(h.dialogs.setRemove.name, "s1");
    assertEquals(h.dialogs.setRemove.users, ["dev"]);
  },
);

Deno.test(
  "setRemoveConfirm: bound users delete with force and still remove locally",
  async () => {
    const h = harness(draftWith("s1", ["m1"]));
    await h.dialogs.setRemove.ask("s1");
    await h.dialogs.setRemove.confirm();
    assertEquals(h.calls.deletes, [["s1", true]]);
    assertEquals(h.calls.removedSets, ["s1"]);
    assertEquals(h.calls.clearedProfileCards, 1);
    assertEquals(h.calls.clearedProviders, []);
    assertEquals(h.dialogs.setRemove.name, null);
  },
);

Deno.test(
  "setRemoveConfirm: a 404 counts as already gone and the local removal runs",
  async () => {
    const h = harness(draftWith("s1", ["m1"]));
    h.failDelete(new KallipaiError({ status: 404, message: "gone" }));
    await h.dialogs.setRemove.ask("s1");
    await h.dialogs.setRemove.confirm();
    assertEquals(h.calls.removedSets, ["s1"]);
    assertEquals(h.dialogs.setRemove.error, null);
  },
);

Deno.test(
  "setRemoveConfirm: any other failure surfaces and the set survives",
  async () => {
    const h = harness(draftWith("s1", ["m1"]));
    h.failDelete(new KallipaiError({ status: 500, message: "boom" }));
    await h.dialogs.setRemove.ask("s1");
    await h.dialogs.setRemove.confirm();
    assertEquals(h.calls.removedSets, []);
    assertEquals(h.dialogs.setRemove.error !== null, true);
    assertEquals(h.dialogs.setRemove.name, "s1");
  },
);

// --- provider form and remove ---

Deno.test(
  "providerSave: re-creates a provider the draft dropped, with the form values",
  () => {
    const initial = draftWith("s1", ["m1"]);
    delete (initial.endpoints as Record<string, unknown>)["openai"];
    const h = harness(initial);
    h.dialogs.provider.show({
      id: "openai",
      family: "openai-compatible",
    } as unknown as ProfileProvider);
    h.dialogs.provider.save({
      id: "openai",
      family: "openai-compatible",
      baseUrl: "http://typed",
      modelListBaseUrl: "",
      apiKey: null,
    });
    const ep = h.draft()!.endpoints["openai"]!;
    assertEquals(ep.base_url, "http://typed");
    assertEquals(h.dialogs.provider.open, false);
  },
);

Deno.test(
  "providerForm: create opens a blank form, show opens that provider",
  () => {
    const h = harness(draftWith("s1", ["m1"]));
    const provider = { id: "openai" } as unknown as ProfileProvider;
    h.dialogs.provider.show(provider);
    assertEquals(h.dialogs.provider.open, true);
    assertEquals(h.dialogs.provider.mode, "edit");
    assertEquals(h.dialogs.provider.provider, provider);
    h.dialogs.provider.close();
    h.dialogs.provider.create();
    assertEquals(h.dialogs.provider.open, true);
    assertEquals(h.dialogs.provider.mode, "new");
    assertEquals(h.dialogs.provider.provider, null);
  },
);

Deno.test(
  "providerRemove: confirm removes by id and clears that card's report",
  () => {
    const h = harness(draftWith("s1", ["m1"]));
    const target = { id: "openai" } as unknown as ProfileProvider;
    h.dialogs.providerRemove.ask(target);
    assertEquals(h.dialogs.providerRemove.target, target);
    assertEquals(h.calls.removedProviders, []);
    h.dialogs.providerRemove.confirm();
    assertEquals(h.calls.removedProviders, ["openai"]);
    assertEquals(h.calls.clearedProviders, ["openai"]);
    assertEquals(h.dialogs.providerRemove.target, null);
  },
);

Deno.test("providerRemove: cancel removes nothing", () => {
  const h = harness(draftWith("s1", ["m1"]));
  h.dialogs.providerRemove.ask({ id: "openai" } as unknown as ProfileProvider);
  h.dialogs.providerRemove.cancel();
  assertEquals(h.dialogs.providerRemove.target, null);
  assertEquals(h.calls.removedProviders, []);
});

// --- set member profile form ---

Deno.test(
  "memberSave: writes at its index and leaves the other rows alone",
  () => {
    const h = harness(draftWith("s1", ["m1", "m2"]));
    h.dialogs.profile.member.show("s1", 1);
    h.dialogs.profile.member.save(form("m2", "typed"));
    const profiles = h.draft()!.sets.s1.profiles;
    assertEquals(
      profiles.map((p) => p.model),
      ["m1", "typed"],
    );
    assertEquals(h.dialogs.profile.member.open, false);
  },
);

Deno.test(
  "memberSave: a target that left the draft leaves it unchanged and closes",
  () => {
    const after = draftWith("other", ["m1"]);
    const h = harness(draftWith("s1", ["m1"]));
    h.dialogs.profile.member.show("s1", 0);
    // The set leaves the draft while the dialog is open.
    h.store.draft = after;
    h.dialogs.profile.member.save(form("m1", "typed"));
    assertEquals(h.draft(), after);
    assertEquals(h.dialogs.profile.member.open, false);
  },
);

Deno.test(
  "memberTest: probes the form values, never the committed draft",
  async () => {
    const h = harness(draftWith("s1", ["m1"]));
    h.dialogs.profile.member.show("s1", 0);
    await h.dialogs.profile.member.test(form("m1", "typed"));
    assertEquals(h.calls.probeDrafts.length, 1);
    assertEquals(
      h.calls.probeDrafts[0].draft.sets.s1.profiles[0].model,
      "typed",
    );
    assertEquals(h.draft()!.sets.s1.profiles[0].model, "m1");
    assertEquals(h.calls.merged, 1);
    assertEquals(h.dialogs.profile.member.report?.status, "ok");
  },
);

// --- unassigned profile form ---

Deno.test(
  "unassignedSave: edit overwrites its index and keeps the rest",
  () => {
    const h = harness(draftWith("s1", ["m1"], ["u1", "u2"]));
    h.dialogs.profile.unassigned.show(0);
    h.dialogs.profile.unassigned.save(form("u1", "typed"));
    assertEquals(
      h.draft()!.parking?.map((p) => p.model),
      ["typed", "u2"],
    );
    assertEquals(h.calls.refreshed, 1);
  },
);

Deno.test("unassignedSave: new appends to the live list", () => {
  const h = harness(draftWith("s1", ["m1"], ["u1"]));
  h.dialogs.profile.unassigned.create();
  h.dialogs.profile.unassigned.save(form("u2", "typed"));
  assertEquals(h.draft()!.parking?.length, 2);
  assertEquals(h.draft()!.parking?.[1].model, "typed");
});

Deno.test("unassignedRemove: drops the row and forgets its p: report", () => {
  const h = harness(draftWith("s1", ["m1"], ["u1", "u2"]));
  h.dialogs.profile.unassigned.show(0);
  h.dialogs.profile.unassigned.remove();
  assertEquals(
    h.draft()!.parking?.map((p) => p.id),
    ["u2"],
  );
  assertEquals(h.calls.clearedUnassigned, ["u1"]);
  assertEquals(h.dialogs.profile.unassigned.open, false);
});

Deno.test(
  "memberForm: opening drops its own report and resets the probe",
  async () => {
    const h = harness(draftWith("s1", ["m1"], ["u1"]));
    h.dialogs.profile.member.show("s1", 0);
    await h.dialogs.profile.member.test(form("m1", "typed"));
    assertEquals(h.dialogs.profile.member.report !== null, true);
    h.dialogs.profile.member.close();
    h.dialogs.profile.member.show("s1", 0);
    assertEquals(h.dialogs.profile.member.report, null);
    assertEquals(h.calls.resetProbe, 2);
  },
);

Deno.test(
  "unassignedForm: opening drops its own report and resets the probe",
  async () => {
    const h = harness(draftWith("s1", ["m1"], ["u1"]));
    h.dialogs.profile.unassigned.show(0);
    await h.dialogs.profile.unassigned.test(form("u1", "typed"));
    assertEquals(h.dialogs.profile.unassigned.report !== null, true);
    h.dialogs.profile.unassigned.show(0);
    assertEquals(h.dialogs.profile.unassigned.report, null);
    assertEquals(h.calls.resetProbe, 2);
  },
);
