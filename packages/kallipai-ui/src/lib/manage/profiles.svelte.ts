// Profiles store: model profile config (named sets + endpoints) with local
// save (PUT), apply (POST /profiles/apply), and probe (POST /profiles/probe).

import type {
  ProfileConfig,
  ProfileProbeRequest,
  ProfileProbeResponse,
  ProfileSourceMode,
  ProfileSourceSelection,
} from "@kallipai/kallipai-client";
import {
  addProfile as addProfileFn,
  addProvider as addProviderFn,
  addSet as addSetFn,
  buildProbeRequest as buildProbeRequestFn,
  profileConfigEqual,
  profileConfigToWire as profileConfigToWireFn,
  removeProfile as removeProfileFn,
  removeProvider as removeProviderFn,
  removeSet as removeSetFn,
  singleProviderProbeRequest as singleProviderProbeRequestFn,
} from "./compute.ts";
import { type ManagementBackend, managementBackend } from "./client.ts";
import { classifySaveFailure, type PendingDangling } from "./profiles-view.ts";
import { KallipaiError } from "@kallipai/kallipai-common";
import { displayError } from "./errors.ts";
import {
  manage_profiles_apply_failed,
  manage_profiles_load_failed,
  manage_profiles_probe_failed,
  manage_profiles_refetch_failed,
  manage_profiles_save_failed,
  manage_profiles_save_stale_backend,
  manage_profiles_switch_failed,
} from "../../paraglide/messages.js";

export class ProfilesStore {
  private _backend: ManagementBackend | null = null;

  private get backend(): ManagementBackend {
    if (this._backend === null) this._backend = managementBackend();
    return this._backend;
  }
  /** The loaded (committed) config from the server. */
  config = $state<ProfileConfig | null>(null);
  /** The local editable copy — diverges from config when dirty. */
  draft = $state<ProfileConfig | null>(null);
  isLoading = $state(false);
  isSaving = $state(false);
  /** The parked dangling-409 state: the stranded set names plus
   * which send parked it. The confirm dialog re-sends by the
   * recorded kind, so the retry always matches the live park. */
  pendingDangling = $state<PendingDangling | null>(null);
  /** True when a force save hit an old backend (bare 409 without the
   * structured list): the confirm re-send is blocked until reload. */
  saveBlocked = $state(false);
  error = $state<string | null>(null);
  isProbing = $state(false);
  isRefetching = $state(false);

  /** Latest probe outcome (POST /profiles/probe), or null. */
  probe = $state<ProfileProbeResponse | null>(null);
  /** Error from the last probe request (HTTP/network level), or null. */
  probeError = $state<string | null>(null);
  /** True when draft diverges from the committed config. */
  get isDirty(): boolean {
    if (!this.config || !this.draft) return false;
    return !profileConfigEqual(this.config, this.draft);
  }

  get hasData(): boolean {
    return this.draft !== null;
  }

  async refresh(): Promise<void> {
    this.isLoading = true;
    this.error = null;
    try {
      const resp = await this.backend.getProfiles();
      this.config = resp;
      this.draft = structuredClone(resp);
    } catch (e) {
      this.error = displayError("profiles", e, manage_profiles_load_failed());
    } finally {
      this.isLoading = false;
    }
  }

  /** Save changes to the server (PUT /profiles). Does NOT affect running
   * agents. With `force`, a dangling-bindings 409 is overridden; without
   * it, a 409 parks the stranded list in `pendingDangling` for the
   * confirm dialog instead of surfacing a generic error. */
  async save(force = false): Promise<void> {
    if (!this.draft) return;
    this.isSaving = true;
    this.error = null;
    this.saveBlocked = false;
    this.pendingDangling = null;
    try {
      const wire = profileConfigToWireFn(this.draft);
      const resp = await this.backend.updateProfiles(
        force ? { ...wire, force: true } : wire,
      );
      this.config = resp;
      this.draft = structuredClone(resp);
    } catch (e) {
      switch (classifySaveFailure(e, force)) {
        case "park-dangling": {
          // Structured 409: ask the operator before stranding bindings.
          const dangling = (e as KallipaiError).api.dangling;
          this.pendingDangling =
            dangling !== undefined ? { names: dangling, retry: "save" } : null;
          break;
        }
        case "stale-backend":
          // Old backend: force is ignored, so a dangling save keeps
          // failing with a bare 409 and no structured list. Surface the
          // upgrade hint and block the confirm re-send instead of
          // looping silently.
          this.saveBlocked = true;
          this.error = manage_profiles_save_stale_backend();
          break;
        default:
          // The raw server text (409 hints, protocol wording) is for the
          // console; the UI only ever shows the designed translation.
          console.warn("[profiles] save failed:", e);
          this.error = manage_profiles_save_failed();
      }
      throw e;
    } finally {
      this.isSaving = false;
    }
  }

  /** Drop the pending dangling list, keeping the edit state as-is. */
  dismissDangling(): void {
    this.pendingDangling = null;
  }

  /** Switch the profile source (PUT /profiles with a source block).
   * A mode different from the live one takes the server's switch
   * branch (a validating gateway fetch for model-gateway, the
   * dangling-bindings gate, then the settings writes and the
   * hot-swap); the profiles payload of the request is ignored by
   * design. `polis` names the target platform (the online UI
   * carries the serving platform's origin); `collection` names
   * the collection to pull (absent keeps the selected one; the
   * gateway holds the selection). On success the
   * response carries the new-source config, which becomes both
   * config and draft. A dangling-bindings 409 parks the stranded
   * list in `pendingDangling` (the confirm dialog re-sends with
   * `force`), like the save path. A failure switches nothing and
   * leaves the local state untouched. */
  async switchSource(
    mode: ProfileSourceMode,
    options: {
      polis?: string;
      collection?: ProfileSourceSelection;
      force?: boolean;
    } = {},
  ): Promise<void> {
    this.isSaving = true;
    this.pendingDangling = null;
    this.error = null;
    try {
      const resp = await this.backend.updateProfiles({
        sets: [],
        endpoints: {},
        parking: [],
        source: {
          mode,
          ...(options.polis !== undefined ? { polis: options.polis } : {}),
          ...(options.collection !== undefined
            ? { collection: options.collection }
            : {}),
        },
        ...(options.force ? { force: true } : {}),
      });
      this.config = resp;
      this.draft = structuredClone(resp);
    } catch (e) {
      switch (classifySaveFailure(e, options.force ?? false)) {
        case "park-dangling": {
          const dangling = (e as KallipaiError).api.dangling;
          this.pendingDangling =
            dangling !== undefined
              ? { names: dangling, retry: "switch" }
              : null;
          break;
        }
        default:
          console.warn("[profiles] source switch failed:", e);
          this.error = displayError(
            "profiles",
            e,
            manage_profiles_switch_failed(),
          );
      }
      throw e;
    } finally {
      this.isSaving = false;
    }
  }

  /** Point the live source at another collection (PUT /profiles
   * with the same source.mode carrying collection). The server
   * writes the pick through to the gateway and re-fetches the
   * snapshot on it (the same dangling-bindings gate applies,
   * and any later rejection names the moved pointer). The
   * response is the same face as GET /profiles, so config and
   * draft both refresh; a dangling-bindings 409 parks the
   * stranded list in `pendingDangling` (the confirm dialog
   * re-sends with `force`). */
  async updateSourceCollection(
    target: ProfileSourceSelection,
    force = false,
  ): Promise<void> {
    this.isSaving = true;
    this.pendingDangling = null;
    this.error = null;
    try {
      const resp = await this.backend.updateProfiles({
        sets: [],
        endpoints: {},
        parking: [],
        source: {
          mode: this.config?.source?.mode ?? "local",
          collection: target,
        },
        ...(force ? { force: true } : {}),
      });
      this.config = resp;
      this.draft = structuredClone(resp);
    } catch (e) {
      switch (classifySaveFailure(e, force)) {
        case "park-dangling": {
          const dangling = (e as KallipaiError).api.dangling;
          this.pendingDangling =
            dangling !== undefined ? { names: dangling, retry: "apply" } : null;
          break;
        }
        default:
          console.warn("[profiles] collection selection failed:", e);
          this.error = displayError(
            "profiles",
            e,
            manage_profiles_switch_failed(),
          );
      }
      throw e;
    } finally {
      this.isSaving = false;
    }
  }

  /** Re-pull the live gateway snapshot (POST /profiles/refresh) and
   * adopt it as both config and draft. Gateway-side edits (a
   * re-anchored default, a published member) reach the running tagma
   * through this; until then it serves the boot snapshot. A local
   * source is refused by the server (409). */
  async refetchSource(): Promise<void> {
    this.isRefetching = true;
    this.error = null;
    try {
      const resp = await this.backend.refreshProfiles();
      this.config = resp;
      this.draft = structuredClone(resp);
    } catch (e) {
      console.warn("[profiles] source refetch failed:", e);
      this.error = displayError(
        "profiles",
        e,
        manage_profiles_refetch_failed(),
      );
      throw e;
    } finally {
      this.isRefetching = false;
    }
  }

  /** Apply the current registry to all live agents (POST /profiles/apply). */
  async apply(): Promise<{ applied: number; skipped: number }> {
    this.isSaving = true;
    this.error = null;
    try {
      const resp = await this.backend.applyProfiles();
      return resp;
    } catch (e) {
      this.error = displayError("profiles", e, manage_profiles_apply_failed());
      throw e;
    } finally {
      this.isSaving = false;
    }
  }

  /**
   * Build a probe request from the draft: endpoints not carrying a freshly typed
   * key probe with the live key (api_key: null), so the masked value from GET is
   * never sent as a credential. `setName` probes a single set; omit for all.
   */
  private buildProbeRequest(setName?: string): ProfileProbeRequest | null {
    if (!this.draft) return null;
    return buildProbeRequestFn(this.config, this.draft, setName);
  }

  private async runProbe(setName?: string): Promise<void> {
    const body = this.buildProbeRequest(setName);
    if (!body) return;
    this.isProbing = true;
    this.probeError = null;
    try {
      this.probe = await this.backend.probeProfiles(body);
    } catch (e) {
      this.probeError = displayError(
        "probe",
        e,
        manage_profiles_probe_failed(),
      );
      this.probe = null;
    } finally {
      this.isProbing = false;
    }
  }

  /** Probe every provider in the draft. */
  probeAll(): Promise<void> {
    return this.runProbe();
  }

  /** Probe the endpoints referenced by one set. */
  probeSet(setName: string): Promise<void> {
    return this.runProbe(setName);
  }

  /** Probe a single provider (no set checks). */
  async probeProvider(id: string): Promise<void> {
    if (!this.draft) return;
    const body = singleProviderProbeRequestFn(this.config, this.draft, id);
    if (!body) return;
    this.isProbing = true;
    this.probeError = null;
    try {
      this.probe = await this.backend.probeProfiles(body);
    } catch (e) {
      this.probeError = displayError(
        "probe",
        e,
        manage_profiles_probe_failed(),
      );
      this.probe = null;
    } finally {
      this.isProbing = false;
    }
  }

  /**
   * Run an arbitrary probe request (single-profile Test) and return the
   * raw response; the page routes the reports inline. Null on failure
   * (probeError then carries the reason).
   */
  async probeRaw(
    body: ProfileProbeRequest,
  ): Promise<ProfileProbeResponse | null> {
    this.isProbing = true;
    this.probeError = null;
    try {
      this.probe = await this.backend.probeProfiles(body);
      return this.probe;
    } catch (e) {
      this.probeError = displayError(
        "probe",
        e,
        manage_profiles_probe_failed(),
      );
      this.probe = null;
      return null;
    } finally {
      this.isProbing = false;
    }
  }
  /** Discard local changes and revert to the last committed config. */
  reset(): void {
    if (this.config) {
      this.draft = $state.snapshot(this.config);
    }
  }

  /** Switch backend. Resets all state. */
  switchBackend(backend: ManagementBackend): void {
    this._backend = backend;
    this.config = null;
    this.draft = null;
    this.error = null;
    this.isSaving = false;
  }

  // --- draft mutators ---

  /** Append a new set under a generated name (the set dialog can rename
   * it); the first set of an empty config also becomes the default. */
  addSet(): void {
    if (!this.draft) return;
    this.draft = addSetFn(this.draft);
  }

  /** Remove the named set (callers gate behind a confirm — removal can
   * strand agents bound to the name). */
  removeSet(name: string): void {
    if (!this.draft) return;
    this.draft = removeSetFn(this.draft, name);
  }

  /** Add a profile to the named set. */
  addProfile(setName: string): void {
    if (!this.draft) return;
    this.draft = addProfileFn(this.draft, setName);
  }

  /** Remove a profile from a set. */
  removeProfile(setName: string, profileIdx: number): void {
    if (!this.draft) return;
    this.draft = removeProfileFn(this.draft, setName, profileIdx);
  }

  /** Add a new provider with a generated id. */
  addProvider(): void {
    if (!this.draft) return;
    const id =
      typeof crypto !== "undefined" && crypto.randomUUID
        ? crypto.randomUUID()
        : `ep-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    this.draft = addProviderFn(this.draft, id);
  }

  /** Remove a provider by id. */
  removeProvider(id: string): void {
    if (!this.draft) return;
    this.draft = removeProviderFn(this.draft, id);
  }
}

export const profilesStore = new ProfilesStore();
