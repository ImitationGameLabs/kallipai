// Instances store: the read-only face of the offline home. Polls the
// local instances service for the daemon's liveness and its instance list;
// spawn/stop mutations rethrow their classified errors for the calling
// surface (form, confirm dialog) to render; the list refreshes on success.

import {
  type InstanceHealth,
  type InstanceInfo,
  instancesClientOrFail,
  InstancesError,
  type InstancesErrorKind,
  type InstanceSpawnInput,
  type InstanceSpawnResult,
} from "./client.ts";
import { manage_instances_load_failed } from "../../paraglide/messages.js";
import { startVisibleInterval } from "../visibleInterval.ts";

class InstancesStore {
  private get client() {
    return instancesClientOrFail();
  }
  private pollStop: (() => void) | null = null;

  health = $state<InstanceHealth | null>(null);
  instances = $state<InstanceInfo[]>([]);
  isLoading = $state(false);
  error = $state<string | null>(null);
  errorKind = $state<InstancesErrorKind | null>(null);
  /** True once any refresh has succeeded; gates the first-frame loading
   * line so the 5s poll never flashes it over live data.
   */
  loaded = $state(false);
  /** The service's machine-readable error code (e.g. host_forbidden). */
  errorCode = $state<string | null>(null);
  /** Provisioning methods the backend advertises; null = unknown or the
   * service is unreachable (the nav entry hides), [] = reachable with no
   * create capability (the entry stays, the create entry hides).
   */
  capabilities = $state<string[] | null>(null);
  /** The listen port of each instance spawned in this session, by slug.
   * The list wire carries the port while running; this memory is the
   * fallback for the spawn-response-to-refresh gap (and legacy daemons
   * without the wire field).
   */
  spawnedPorts = $state<Record<string, number>>({});

  /** Fetch both faces once; a classified error lands in error/errorKind. */
  async refresh(): Promise<void> {
    this.isLoading = true;
    try {
      const [health, instances] = await Promise.all([
        this.client.health(),
        this.client.list(),
      ]);
      void this.fetchCapabilities();
      this.health = health;
      this.instances = instances;
      this.error = null;
      this.errorCode = null;
      this.errorKind = null;
      this.loaded = true;
    } catch (cause) {
      if (cause instanceof InstancesError) {
        this.errorKind = cause.kind;
        this.errorCode = cause.code ?? null;
        this.error = cause.message;
      } else {
        this.errorKind = "other";
        this.error = manage_instances_load_failed();
      }
    } finally {
      this.isLoading = false;
    }
  }
  /** One capability probe; best-effort (failure leaves the entry hidden).
   * Called from refresh and once at layout boot (the nav entry needs it
   * before the instances page is ever visited).
   */
  async fetchCapabilities(): Promise<void> {
    try {
      this.capabilities = await this.client.capabilities();
    } catch {
      this.capabilities = null;
    }
  }

  /** Spawn one instance; success records its port and refreshes the list.
   * Errors rethrow classified for the form to render.
   */
  async spawn(input: InstanceSpawnInput): Promise<InstanceSpawnResult> {
    const result = await this.client.spawn(input);
    this.spawnedPorts[input.slug] = result.port;
    await this.refresh();
    return result;
  }

  /** Stop one instance by slug; errors rethrow for the dialog to render. */
  async stop(slug: string): Promise<void> {
    await this.client.stop(slug);
    await this.refresh();
  }

  /** Relaunch a stopped or dead instance; success records the fresh port
   * (same session-gap fallback as spawn) and refreshes. Errors rethrow
   * classified for the calling card to render.
   */
  async start(slug: string): Promise<void> {
    const result = await this.client.start(slug);
    this.spawnedPorts[slug] = result.port;
    await this.refresh();
  }

  startPolling(intervalMs = 5000): void {
    this.stopPolling();
    this.refresh();
    this.pollStop = startVisibleInterval(() => {
      if (!this.isLoading) this.refresh();
    }, intervalMs);
  }

  stopPolling(): void {
    this.pollStop?.();
    this.pollStop = null;
  }
}

export const instancesStore = new InstancesStore();
