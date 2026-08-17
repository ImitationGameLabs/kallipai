// Profiles store: model profile config (tiers + endpoints) with local editing,
// save (PUT), apply (POST /profiles/apply), and probe (POST /profiles/probe).

import type {
  ProfileConfig,
  ProfileProbeRequest,
  ProfileProbeResponse,
} from "@kallipai/kallip-client";
import {
  addProvider as addProviderFn,
  addProfile as addProfileFn,
  addTier as addTierFn,
  buildProbeRequest as buildProbeRequestFn,
  profileConfigEqual,
  profileConfigToWire as profileConfigToWireFn,
  removeProvider as removeProviderFn,
  removeLastTier as removeLastTierFn,
  removeProfile as removeProfileFn,
  singleProviderProbeRequest as singleProviderProbeRequestFn,
} from "./compute.ts";
import { type ManagementBackend, managementBackend } from "./client.ts";

class ProfilesStore {
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
  error = $state<string | null>(null);
  isProbing = $state(false);

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
      this.error = e instanceof Error ? e.message : String(e);
    } finally {
      this.isLoading = false;
    }
  }

  /** Save changes to the server (PUT /profiles). Does NOT affect running agents. */
  async save(): Promise<void> {
    if (!this.draft) return;
    this.isSaving = true;
    this.error = null;
    try {
      const resp = await this.backend.updateProfiles(
        profileConfigToWireFn(this.draft),
      );
      this.config = resp;
      this.draft = structuredClone(resp);
    } catch (e) {
      this.error = e instanceof Error ? e.message : String(e);
      throw e;
    } finally {
      this.isSaving = false;
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
      this.error = e instanceof Error ? e.message : String(e);
      throw e;
    } finally {
      this.isSaving = false;
    }
  }

  /**
   * Build a probe request from the draft: endpoints not carrying a freshly typed
   * key probe with the live key (api_key: null), so the masked value from GET is
   * never sent as a credential. `tierIdx` probes a single tier; omit for all.
   */
  private buildProbeRequest(tierIdx?: number): ProfileProbeRequest | null {
    if (!this.draft) return null;
    return buildProbeRequestFn(this.config, this.draft, tierIdx);
  }

  private async runProbe(tierIdx?: number): Promise<void> {
    const body = this.buildProbeRequest(tierIdx);
    if (!body) return;
    this.isProbing = true;
    this.probeError = null;
    try {
      this.probe = await this.backend.probeProfiles(body);
    } catch (e) {
      this.probeError = e instanceof Error ? e.message : String(e);
      this.probe = null;
    } finally {
      this.isProbing = false;
    }
  }

  /** Probe every provider in the draft. */
  probeAll(): Promise<void> {
    return this.runProbe();
  }

  /** Probe the endpoints referenced by one tier. */
  probeTier(tierIdx: number): Promise<void> {
    return this.runProbe(tierIdx);
  }

  /** Probe a single provider (no tier checks). */
  async probeProvider(id: string): Promise<void> {
    if (!this.draft) return;
    const body = singleProviderProbeRequestFn(this.config, this.draft, id);
    if (!body) return;
    this.isProbing = true;
    this.probeError = null;
    try {
      this.probe = await this.backend.probeProfiles(body);
    } catch (e) {
      this.probeError = e instanceof Error ? e.message : String(e);
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
      this.probeError = e instanceof Error ? e.message : String(e);
      this.probe = null;
      return null;
    } finally {
      this.isProbing = false;
    }
  }
  /** Discard local changes and revert to the last committed config. */
  reset(): void {
    if (this.config) {
      this.draft = structuredClone(this.config);
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

  /** Append a new tier (append-only — tiers are positional). */
  addTier(): void {
    if (!this.draft) return;
    this.draft = addTierFn(this.draft);
  }

  /** Remove the LAST tier only (truncate-tail — middle removal rebinds agents). */
  removeLastTier(): void {
    if (!this.draft) return;
    this.draft = removeLastTierFn(this.draft);
  }

  /** Add a profile to a tier at the given index. */
  addProfile(tierIdx: number): void {
    if (!this.draft) return;
    this.draft = addProfileFn(this.draft, tierIdx);
  }

  /** Remove a profile from a tier. */
  removeProfile(tierIdx: number, profileIdx: number): void {
    if (!this.draft) return;
    this.draft = removeProfileFn(this.draft, tierIdx, profileIdx);
  }

  /** Add a new provider with a generated id. */
  addProvider(): void {
    if (!this.draft) return;
    const id = typeof crypto !== "undefined" && crypto.randomUUID
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
