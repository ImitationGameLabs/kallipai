// Tasks store: the read-only task ledger (lightweight list rows + the
// selected task's detail). Mirrors the agents store's transport wiring:
// backend switching per channel state, live projection dirty feed when
// present, visible-interval polling as the offline backstop. The list
// polls rows only; the detail pane fetches once per selection and again
// when the selected row's status moves under it.

import type {
  TaskExport,
  TaskRow,
  TaskStatus,
  TaskTimeAxis,
} from "@kallipai/kallip-client";
import {
  type ManagementBackend,
  OfflineBackend,
  managementBackend,
} from "./client.ts";
import { startVisibleInterval } from "../visibleInterval.ts";
import { clampPage, filterEpoch } from "./compute.ts";
import { displayError } from "./errors.ts";
import {
  manage_tasks_load_failed,
  manage_tasks_download_failed,
} from "../../paraglide/messages.js";

/** Server-side page size for the row list. */
export const TASKS_PAGE_SIZE = 25;

class TasksStore {
  private _backend: ManagementBackend | null = null;

  private get backend(): ManagementBackend {
    if (this._backend === null) this._backend = managementBackend();
    return this._backend;
  }

  rows = $state<TaskRow[]>([]);
  total = $state(0);
  page = $state(0);
  selectedId = $state<number | null>(null);
  detail = $state<TaskExport | null>(null);
  showArchived = $state(false);
  statusFilter = $state<TaskStatus | null>(null);
  assigneeFilter = $state("");
  timeAxis = $state<TaskTimeAxis>("updated");
  sinceDate = $state("");
  untilDate = $state("");
  isLoading = $state(false);
  hasLoaded = $state(false);
  error = $state<string | null>(null);
  /** Large downloads need the direct transport; the relay-tunnel
   * manage-reply frame caps response bodies below ledger exports. */
  downloadsAvailable = $state(true);
  downloading = $state(false);

  private pollStop: (() => void) | null = null;
  /** Monotonic token for in-flight refreshes: a slow response from a
   * superseded request must not clobber the newer one's rows (the
   * same race loadDetail guards by selected id). */
  private refreshSeq = 0;

  async refresh(force = false): Promise<void> {
    if (!force && this.isLoading) return;
    const seq = ++this.refreshSeq;
    this.isLoading = true;
    this.error = null;
    const superseded = () => seq !== this.refreshSeq;

    try {
      const result = await this.backend.listTasks({
        archived: this.showArchived,
        status: this.statusFilter ?? undefined,
        assignee: this.assigneeFilter.trim() || undefined,
        time: this.timeAxis,
        since: filterEpoch(this.sinceDate),
        until: filterEpoch(this.untilDate, true),
        limit: TASKS_PAGE_SIZE,
        offset: this.page * TASKS_PAGE_SIZE,
      });
      if (superseded()) return;
      const prev = this.rows.find((r) => r.id === this.selectedId);
      // Re-clamp the page against the fresh total: a shrinking ledger
      // can strand the current page past the new end.
      const clamped = clampPage(this.page, result.total, TASKS_PAGE_SIZE);
      const pageMoved = clamped !== this.page;
      this.page = clamped;
      this.rows = [...result.rows];
      this.total = result.total;
      if (pageMoved) {
        // The rows just read belong to the stranded page; refetch the
        // clamped page (forced, bounded: the clamped page is in range).
        void this.refresh(true);
        return;
      } else if (superseded()) {
        return;
      }
      // Keep the selection stable; fall back to the first row when it
      // disappeared from the current partition (e.g. the archive toggle).
      const ids = new Set(this.rows.map((t) => t.id));
      if (this.selectedId === null || !ids.has(this.selectedId)) {
        this.selectedId = this.rows[0]?.id ?? null;
        if (this.selectedId !== null) void this.loadDetail(this.selectedId);
      } else if (prev && this.selectedId !== null) {
        // The selected row updated: if its updated_at moved (status,
        // confirmations, notes), refresh the detail pane once so it stays
        // consistent with the list.
        const now = this.rows.find((r) => r.id === this.selectedId);
        if (now && now.updated_at !== prev.updated_at) {
          void this.loadDetail(this.selectedId);
        }
      }
      this.hasLoaded = true;
    } catch (e) {
      if (superseded()) return;
      this.error = displayError("tasks", e, manage_tasks_load_failed());
    } finally {
      if (seq === this.refreshSeq) this.isLoading = false;
    }
  }

  select(id: number): void {
    this.selectedId = id;
    void this.loadDetail(id);
  }

  /** Download the closed dossier snapshot (direct transport only). */
  async fetchArchive(id: number): Promise<Uint8Array | null> {
    this.downloading = true;
    try {
      return await this.backend.fetchTaskArchive(id);
    } catch (e) {
      this.error = displayError(
        "archive download",
        e,
        manage_tasks_download_failed(),
      );
      return null;
    } finally {
      this.downloading = false;
    }
  }

  /** Export one task plus its trail as JSON (direct transport only). */
  async exportTask(id: number): Promise<TaskExport | null> {
    this.downloading = true;
    try {
      return await this.backend.exportTask(id);
    } catch (e) {
      this.error = displayError(
        "task export",
        e,
        manage_tasks_download_failed(),
      );
      return null;
    } finally {
      this.downloading = false;
    }
  }

  /** Export every task plus its trail as JSON (direct transport only). */
  async exportAllTasks(): Promise<TaskExport[] | null> {
    this.downloading = true;
    try {
      return await this.backend.exportAllTasks();
    } catch (e) {
      this.error = displayError(
        "bulk export",
        e,
        manage_tasks_download_failed(),
      );
      return null;
    } finally {
      this.downloading = false;
    }
  }

  private async loadDetail(id: number): Promise<void> {
    try {
      const detail = await this.backend.getTask(id);
      // A slower earlier request must not overwrite the newer selection.
      if (this.selectedId !== id) {
        return;
      }
      this.detail = detail;
    } catch (e) {
      if (this.selectedId !== id) {
        return;
      }
      this.error = displayError("task detail", e, manage_tasks_load_failed());
    }
  }

  /** Set one list filter and refetch from the first page. */
  setStatus(status: TaskStatus | null): void {
    this.statusFilter = status;
    this.page = 0;
    void this.refresh(true);
  }

  setAssignee(assignee: string): void {
    this.assigneeFilter = assignee;
    this.page = 0;
    void this.refresh(true);
  }

  setTimeAxis(timeAxis: TaskTimeAxis): void {
    this.timeAxis = timeAxis;
    this.page = 0;
    void this.refresh(true);
  }

  setSince(date: string): void {
    this.sinceDate = date;
    this.page = 0;
    void this.refresh(true);
  }

  setUntil(date: string): void {
    this.untilDate = date;
    this.page = 0;
    void this.refresh(true);
  }

  /** Flip the archive partition (mirrors the CLI's --archived flag). */
  toggleArchived(): void {
    this.showArchived = !this.showArchived;
    this.page = 0;
    void this.refresh(true);
  }

  /** Jump to a page (zero-based), clamped into the valid range. */
  goToPage(p: number): void {
    const clamped = clampPage(p, this.total, TASKS_PAGE_SIZE);
    if (clamped === this.page) return;
    this.page = clamped;
    void this.refresh(true);
  }

  startPolling(intervalMs = 30_000): void {
    this.stopPolling();
    const feed = this.backend.projectionFeed;
    if (feed) {
      this.pollStop = feed.subscribe(() => void this.refresh());
    } else {
      this.pollStop = startVisibleInterval(() => this.refresh(), intervalMs);
    }
    this.refresh();
  }

  stopPolling(): void {
    this.pollStop?.();
    this.pollStop = null;
  }

  /** Switch backend. Resets all state. */
  switchBackend(backend: ManagementBackend): void {
    this.stopPolling();
    this._backend = backend;
    this.downloadsAvailable = backend instanceof OfflineBackend;
    this.refreshSeq = 0;
    this.isLoading = false;
    this.rows = [];
    this.total = 0;
    this.page = 0;
    this.selectedId = null;
    this.detail = null;
    this.showArchived = false;
    this.statusFilter = null;
    this.assigneeFilter = "";
    this.timeAxis = "updated";
    this.sinceDate = "";
    this.untilDate = "";
    this.error = null;
    this.hasLoaded = false;
  }
}

export const tasksStore = new TasksStore();
