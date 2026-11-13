<script lang="ts">
  // Personal access tokens: list, mint, rescope, revoke. The page body is the
  // list of live tokens; minting, rescoping and revoking each open in a dialog,
  // so no form keeps a permanent row of fields beside the list. A revoked token
  // leaves this list, and the server still returns it to anyone who asks for
  // the full set. One fact the copy has to carry, because the surface cannot
  // show it by itself: a permission change lands on the token's next request.
  import TokenCreateDialog from "./TokenCreateDialog.svelte";
  import TokenEditDialog from "./TokenEditDialog.svelte";
  import NewSecretPanel from "../NewSecretPanel.svelte";
  import ConfirmDialog from "../ConfirmDialog.svelte";
  import { archeionSession } from "../../lib/session/archeion.svelte";
  import {
    ArcheionApiError,
    type TokenView,
  } from "@kallipai/kallipai-archeion-client";
  import { permLabel, splitPerms } from "../../lib/perms.ts";
  import { formatDateTime } from "../../lib/tagmata.svelte.ts";
  import { timezoneSetting } from "../../lib/time/stamp.svelte.ts";
  import {
    auth_couldnt_reach,
    account_tokens_copy,
    account_tokens_create,
    account_tokens_create_failed,
    account_tokens_created,
    account_tokens_edit,
    account_tokens_effect,
    account_tokens_expires,
    account_tokens_intro,
    account_tokens_last_used,
    account_tokens_never_used,
    account_tokens_none,
    account_tokens_revoke,
    account_tokens_revoke_desc,
    account_tokens_secret_desc,
    account_tokens_secret_title,
    account_tokens_write_failed,
  } from "../../paraglide/messages.js";

  const tokens = $derived(archeionSession.tokens.filter((t) => !t.revoked_at));

  // The one plaintext, held until the owner dismisses it.
  let issued = $state<{ id: string; plaintext: string } | null>(null);
  let createOpen = $state(false);
  let creating = $state(false);
  let createError = $state<string | null>(null);
  let editing = $state<TokenView | null>(null);
  // The row whose edit request is out, or null. The window closes while
  // the request runs, so the lock reads this, not the window's token.
  let editBusyId = $state<string | null>(null);
  const editingBusy = $derived(editBusyId !== null);
  let editError = $state<string | null>(null);
  let revoking = $state<TokenView | null>(null);
  // Kept after the window closes, so the question still names its token.
  let revokeName = $state("");
  // The row whose revoke request is out, or null. Kept apart from the dialog
  // window: a cancel closes the window, the request keeps running, and the
  // lock has to outlast both.
  let revokeBusyId = $state<string | null>(null);
  let revokeError = $state<string | null>(null);
  // The failure line for a write whose dialog is already gone: the reason has
  // no reader in the dialog by then, so it lands here and names its token.
  let error = $state<string | null>(null);
  let notice = $state<string | null>(null);

  function msgOf(e: unknown): string {
    if (e instanceof ArcheionApiError) return e.message;
    console.error(e);
    return auth_couldnt_reach();
  }

  function stamp(iso: string | null): string {
    return iso ? formatDateTime(iso, timezoneSetting.value) : "";
  }

  // One write at a time per row: an edit in flight locks that row's revoke key
  // and the reverse, so two requests never race on the same token.
  function locked(id: string): boolean {
    return editBusyId === id || revokeBusyId === id;
  }

  async function create(mint: {
    name: string;
    perms: string[];
    days: number;
  }): Promise<void> {
    if (creating) return;
    createError = null;
    creating = true;
    try {
      const result = await archeionSession.createToken({
        name: mint.name,
        perms: mint.perms,
        expires_in_days: mint.days,
      });
      issued = { id: result.token.id, plaintext: result.plaintext };
      createOpen = false;
      error = null;
      notice = null;
    } catch (e) {
      // Cancelling the dialog does not stop the request, so the dialog may be
      // gone when the refusal arrives.
      if (createOpen) createError = msgOf(e);
      else
        error = account_tokens_create_failed({
          name: mint.name,
          error: msgOf(e),
        });
    } finally {
      creating = false;
    }
  }

  async function saveEdit(row: TokenView, perms: string[]): Promise<void> {
    if (editBusyId !== null) return;
    if (revokeBusyId === row.id) return;
    editBusyId = row.id;
    editError = null;
    try {
      await archeionSession.updateToken(row.id, { perms });
      editing = null;
      error = null;
      notice = account_tokens_effect();
    } catch (e) {
      // The dialog carries the reason while it still shows this row; a reason
      // for a row it no longer shows goes to the page line.
      if (editing?.id === row.id) editError = msgOf(e);
      else
        error = account_tokens_write_failed({
          name: row.name,
          error: msgOf(e),
        });
    } finally {
      editBusyId = null;
    }
  }

  async function revoke(row: TokenView): Promise<void> {
    if (revokeBusyId !== null) return;
    if (editBusyId === row.id) return;
    revokeBusyId = row.id;
    revokeError = null;
    try {
      await archeionSession.revokeToken(row.id);
      if (revoking?.id === row.id) revoking = null;
      if (editing?.id === row.id) editing = null;
      error = null;
      notice = null;
    } catch (e) {
      // The shell keeps its window and its cancel key shut while the request
      // runs, so the dialog is still showing this row and owns the reason.
      revokeError = msgOf(e);
    } finally {
      revokeBusyId = null;
    }
  }
</script>

<section class="space-y-3">
  <div class="card preset-tonal-surface p-4 space-y-3">
    <div class="flex flex-wrap items-start justify-between gap-3">
      <div class="min-w-0 space-y-1">
        <p class="text-xs opacity-60">{account_tokens_intro()}</p>
        <p class="text-xs opacity-60">{account_tokens_effect()}</p>
      </div>
      <button
        type="button"
        class="btn shrink-0 preset-filled-primary-500 text-sm"
        onclick={() => {
          createError = null;
          createOpen = true;
        }}>{account_tokens_create()}</button
      >
    </div>

    {#if issued}
      <NewSecretPanel
        title={account_tokens_secret_title()}
        description={account_tokens_secret_desc()}
        secret={issued.plaintext}
        copyLabel={account_tokens_copy()}
        onDismiss={() => (issued = null)}
      />
    {/if}

    {#if error}
      <p role="alert" class="text-xs text-error-500 dark:text-error-400">
        {error}
      </p>
    {/if}
    {#if notice}
      <p class="text-xs opacity-70">{notice}</p>
    {/if}

    {#if tokens.length === 0}
      <p class="text-sm opacity-60">{account_tokens_none()}</p>
    {:else}
      <ul class="space-y-3">
        {#each tokens as t (t.id)}
          {@const parts = splitPerms(t.perms)}
          <li
            class="flex flex-col gap-2 border-t border-surface-500/20 pt-3 first:border-0 first:pt-0 md:flex-row md:items-start md:justify-between"
          >
            <div class="min-w-0 space-y-2">
              <div class="flex flex-wrap items-center gap-2 text-sm">
                <span class="font-medium break-all">{t.name}</span>
                {#if t.masked}
                  <span class="font-mono text-xs opacity-60 break-all"
                    >{t.masked}</span
                  >
                {/if}
              </div>

              <div class="flex flex-wrap gap-1">
                {#each parts.known as p (p)}
                  <span class="badge preset-tonal-surface text-xs"
                    >{permLabel(p)}</span
                  >
                {/each}
                {#each parts.unknown as p (p)}
                  <span class="badge preset-tonal-surface text-xs font-mono"
                    >{p}</span
                  >
                {/each}
              </div>

              <div class="flex flex-wrap gap-x-3 gap-y-1 text-xs opacity-60">
                <span
                  >{account_tokens_created({ date: stamp(t.created_at) })}</span
                >
                {#if t.expires_at}
                  <span
                    >{account_tokens_expires({
                      date: stamp(t.expires_at),
                    })}</span
                  >
                {/if}
                <span
                  >{t.last_used_at
                    ? account_tokens_last_used({ date: stamp(t.last_used_at) })
                    : account_tokens_never_used()}</span
                >
              </div>
            </div>

            <div class="flex flex-wrap gap-2 md:shrink-0">
              <button
                type="button"
                class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
                disabled={locked(t.id)}
                onclick={() => {
                  editError = null;
                  editing = t;
                }}>{account_tokens_edit()}</button
              >
              <button
                type="button"
                class="btn btn-sm preset-outlined-error-500 hover:preset-filled-error-500"
                disabled={locked(t.id)}
                onclick={() => {
                  revokeError = null;
                  revoking = t;
                  revokeName = t.name;
                }}>{account_tokens_revoke()}</button
              >
            </div>
          </li>
        {/each}
      </ul>
    {/if}
  </div>
</section>

<TokenCreateDialog
  open={createOpen}
  busy={creating}
  error={createError}
  onSave={create}
  onCancel={() => (createOpen = false)}
/>

<TokenEditDialog
  open={editing !== null}
  token={editing}
  busy={editingBusy}
  error={editError}
  onSave={(perms) => {
    const row = editing;
    if (row) saveEdit(row, perms);
  }}
  onCancel={() => (editing = null)}
/>

<ConfirmDialog
  open={revoking !== null}
  title={account_tokens_revoke()}
  description={account_tokens_revoke_desc({ name: revokeName })}
  confirmLabel={account_tokens_revoke()}
  tone="danger"
  busy={revokeBusyId !== null}
  error={revokeError}
  onConfirm={() => {
    const row = revoking;
    if (row) revoke(row);
  }}
  onCancel={() => (revoking = null)}
/>
