<script lang="ts">
  // The tagma's access credential: what it may do, what changed, and how to
  // replace the secret. The permission list governs its file-service calls; a
  // tagma reaches no registry route, so registry strings on the record are
  // listed as carried but unreachable. Its room traffic is judged by identity
  // and membership,
  // so the panel names the scope it really covers and says that a rescope or
  // reset lands on the tagma's next scoped call. Reads and writes go straight
  // to the archeion (owner-session routes); the history is read-only and keeps
  // every event, reset included.
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import {
    ArcheionApiError,
    type TagmaCredentialView,
  } from "@kallipai/kallipai-archeion-client";
  import PermChecklist from "../PermChecklist.svelte";
  import NewSecretPanel from "../NewSecretPanel.svelte";
  import { archeionClientOrFail } from "../../lib/session/archeion.svelte";
  import { TAGMA_PERM_IDS, permLabel, splitPerms } from "../../lib/perms.ts";
  import { formatDateTime } from "../../lib/tagmata.svelte.ts";
  import { timezoneSetting } from "../../lib/time/stamp.svelte.ts";
  import {
    auth_couldnt_reach,
    common_cancel,
    common_close,
    common_save,
    tagma_credential_copy,
    tagma_credential_created,
    tagma_credential_effect,
    tagma_credential_empty_perms,
    tagma_credential_event_create,
    tagma_credential_event_perms,
    tagma_credential_event_reset,
    tagma_credential_history,
    tagma_credential_intro,
    tagma_credential_last_used,
    tagma_credential_load_failed,
    tagma_credential_never_used,
    tagma_credential_new_secret,
    tagma_credential_new_secret_hint,
    tagma_credential_perms,
    tagma_credential_reset,
    tagma_credential_reset_desc,
    tagma_credential_reset_title,
    tagma_credential_save,
    tagma_credential_title,
    tagma_credential_unreachable_perms,
  } from "../../paraglide/messages.js";

  let {
    open,
    tagmaId,
    tagmaLabel = null,
    onClose,
  }: {
    open: boolean;
    tagmaId: string;
    tagmaLabel?: string | null;
    onClose: () => void;
  } = $props();

  let data = $state<TagmaCredentialView | null>(null);
  let loadedFor = $state<string | null>(null);
  let loading = $state(false);
  let error = $state<string | null>(null);
  // Every stored string, unknown ones included: an unreachable string stays
  // visible until its carrier removes it, and a save never drops a grant
  // silently.
  let perms = $state<string[]>([]);
  let saving = $state(false);
  let confirmingReset = $state(false);
  let resetting = $state(false);
  let newSecret = $state<string | null>(null);

  const known = $derived(splitPerms(perms));
  const unreachable = $derived(
    known.known.filter((p) => !TAGMA_PERM_IDS.includes(p)),
  );
  const reachable = $derived(TAGMA_PERM_IDS.filter((id) => perms.includes(id)));
  const dirty = $derived.by(() => {
    // The narrowing has to survive into the callback, so bind the record
    // to a local first: a closure does not inherit the null check.
    const record = data;
    if (record === null) return false;
    return (
      perms.length !== record.token.perms.length ||
      perms.some((p) => !record.token.perms.includes(p))
    );
  });

  function msgOf(e: unknown, fallback: () => string): string {
    if (e instanceof ArcheionApiError) return e.message;
    console.error(e);
    return fallback();
  }

  function stamp(iso: string | null): string {
    return iso ? formatDateTime(iso, timezoneSetting.value) : "";
  }

  async function load(id: string): Promise<void> {
    loadedFor = id;
    loading = true;
    error = null;
    try {
      data = await archeionClientOrFail().getTagmaCredential(id);
      perms = [...data.token.perms];
    } catch (e) {
      data = null;
      error = msgOf(e, tagma_credential_load_failed);
    } finally {
      loading = false;
    }
  }

  $effect(() => {
    if (!open) {
      loadedFor = null;
      data = null;
      newSecret = null;
      confirmingReset = false;
      error = null;
      return;
    }
    if (loadedFor !== tagmaId) void load(tagmaId);
  });

  async function savePerms(): Promise<void> {
    if (!data || saving) return;
    saving = true;
    error = null;
    try {
      data = await archeionClientOrFail().updateTagmaCredential(tagmaId, {
        perms,
      });
      perms = [...data.token.perms];
    } catch (e) {
      error = msgOf(e, auth_couldnt_reach);
    } finally {
      saving = false;
    }
  }

  async function resetSecret(): Promise<void> {
    if (resetting) return;
    resetting = true;
    error = null;
    try {
      const issued = await archeionClientOrFail().resetTagmaCredential(tagmaId);
      data = { token: issued.token, events: data?.events ?? [] };
      newSecret = issued.plaintext;
      confirmingReset = false;
      // The reset appends a history row; refetch so the trail shows it.
      await load(tagmaId);
    } catch (e) {
      error = msgOf(e, auth_couldnt_reach);
    } finally {
      resetting = false;
    }
  }

  function eventLabel(kind: string): string {
    if (kind === "create") return tagma_credential_event_create();
    if (kind === "reset") return tagma_credential_event_reset();
    if (kind === "perms") return tagma_credential_event_perms();
    // An unknown kind is shown as stored, the same way an unknown permission
    // string is: calling it a permission change would be a claim the record
    // does not make.
    return kind;
  }

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open && !saving && !resetting) onClose();
  }
</script>

<Dialog {open} {onOpenChange}>
  <Portal>
    <Dialog.Backdrop class="fixed inset-0 bg-surface-50-950/60 z-50" />
    <Dialog.Positioner class="fixed inset-0 z-50 grid place-items-center p-4">
      <Dialog.Content
        class="card preset-tonal-surface w-full max-w-lg p-6 flex flex-col gap-4 max-h-[90vh] overflow-y-auto"
      >
        <Dialog.Title class="text-lg font-semibold">
          {tagma_credential_title()}
        </Dialog.Title>
        <Dialog.Description class="text-xs opacity-70">
          {tagma_credential_intro()}
        </Dialog.Description>

        {#if tagmaLabel}
          <p class="text-sm font-medium break-all">{tagmaLabel}</p>
        {/if}

        {#if loading && !data}
          <p class="text-sm opacity-60">…</p>
        {:else if data}
          <div class="flex flex-wrap items-center gap-2 text-xs opacity-70">
            {#if data.token.masked}
              <span class="font-mono break-all">{data.token.masked}</span>
            {/if}
            <span
              >{tagma_credential_created({
                date: stamp(data.token.created_at),
              })}</span
            >
            <span
              >{data.token.last_used_at
                ? tagma_credential_last_used({
                    date: stamp(data.token.last_used_at),
                  })
                : tagma_credential_never_used()}</span
            >
          </div>

          <section class="space-y-2">
            <h3 class="text-xs font-medium uppercase opacity-60 tracking-wide">
              {tagma_credential_perms()}
            </h3>
            <PermChecklist
              value={perms}
              onChange={(next) => (perms = next)}
              unknown={[...known.unknown, ...unreachable]}
              ids={TAGMA_PERM_IDS}
              disabled={saving}
            />
            {#if unreachable.length > 0}
              <p class="text-xs opacity-60">
                {tagma_credential_unreachable_perms()}
              </p>
            {/if}
            {#if reachable.length === 0}
              <p class="text-xs text-warning-500 dark:text-warning-400">
                {tagma_credential_empty_perms()}
              </p>
            {/if}
            <p class="text-xs opacity-60">{tagma_credential_effect()}</p>
            <button
              type="button"
              class="btn btn-sm preset-filled-primary-500"
              disabled={saving || !dirty}
              onclick={savePerms}>{tagma_credential_save()}</button
            >
          </section>

          <section class="space-y-2 border-t border-surface-500/20 pt-3">
            <h3 class="text-xs font-medium uppercase opacity-60 tracking-wide">
              {tagma_credential_history()}
            </h3>
            <ul class="space-y-1 text-xs">
              {#each data.events as e, i (i)}
                <li class="flex flex-wrap gap-2 opacity-70">
                  <span class="font-medium">{eventLabel(e.kind)}</span>
                  <span>{stamp(e.at)}</span>
                  {#if e.added.length > 0}
                    <span class="preset-text-success-500"
                      >+{e.added.map((p) => permLabel(p)).join(", ")}</span
                    >
                  {/if}
                  {#if e.removed.length > 0}
                    <span class="preset-text-error-500"
                      >-{e.removed.map((p) => permLabel(p)).join(", ")}</span
                    >
                  {/if}
                </li>
              {/each}
            </ul>
          </section>
        {/if}

        {#if newSecret}
          <NewSecretPanel
            title={tagma_credential_new_secret()}
            hint={tagma_credential_new_secret_hint()}
            secret={newSecret}
            copyLabel={tagma_credential_copy()}
          />
        {/if}

        {#if error}
          <p role="alert" class="text-xs text-error-500 dark:text-error-400">
            {error}
          </p>
        {/if}

        {#if confirmingReset}
          <div class="space-y-2 rounded border border-error-500/40 p-3">
            <p class="text-sm font-medium">{tagma_credential_reset_title()}</p>
            <p class="text-xs opacity-70">{tagma_credential_reset_desc()}</p>
            <div class="flex flex-wrap gap-2">
              <button
                type="button"
                class="btn btn-sm preset-filled-error-500"
                disabled={resetting}
                onclick={resetSecret}>{tagma_credential_reset()}</button
              >
              <button
                type="button"
                class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
                disabled={resetting}
                onclick={() => (confirmingReset = false)}
                >{common_cancel()}</button
              >
            </div>
          </div>
        {/if}

        <div class="flex flex-wrap justify-end gap-2">
          {#if !confirmingReset}
            <button
              type="button"
              class="btn btn-sm preset-outlined-error-500 hover:preset-filled-error-500"
              disabled={!data || resetting || saving}
              onclick={() => (confirmingReset = true)}
              >{tagma_credential_reset()}</button
            >
          {/if}
          <button
            type="button"
            class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
            onclick={onClose}>{common_close()}</button
          >
        </div>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
