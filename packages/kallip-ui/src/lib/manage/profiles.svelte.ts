// Profiles store: model profile config (tiers + endpoints) with local editing,
// save (PUT), and apply (POST /profiles/apply).

import type { ProfileConfig } from "@kallipai/kallip-client";
import {
  addTier as addTierFn,
  removeLastTier as removeLastTierFn,
  addProfile as addProfileFn,
  removeProfile as removeProfileFn,
  addEndpoint as addEndpointFn,
  removeEndpoint as removeEndpointFn,
  profileConfigEqual,
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
      const resp = await this.backend.updateProfiles(this.draft);
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

  /** Add a new endpoint with a generated id. */
  addEndpoint(): void {
    if (!this.draft) return;
    const id = typeof crypto !== "undefined" && crypto.randomUUID
      ? crypto.randomUUID()
      : `ep-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    this.draft = addEndpointFn(this.draft, id);
  }

  /** Remove an endpoint by id. */
  removeEndpoint(id: string): void {
    if (!this.draft) return;
    this.draft = removeEndpointFn(this.draft, id);
  }
}

export const profilesStore = new ProfilesStore();
