<script lang="ts">
  // Personal access tokens: list, mint, rescope, revoke. Two facts the copy has
  // to carry, because the surface cannot show them by itself: a permission
  // change lands on the token's next request (nothing restarts), and a revoked
  // token keeps its row with the time it was cut off.
  import PermChecklist from "../PermChecklist.svelte";
  import NewSecretPanel from "../NewSecretPanel.svelte";
  import { archeionSession } from "../../lib/session/archeion.svelte";
  import {
    ArcheionApiError,
    type TokenView,
  } from "@kallipai/kallipai-archeion-client";
  import { PAT_DEFAULT_PERMS, permLabel, splitPerms } from "../../lib/perms.ts";
  import { formatDateTime } from "../../lib/tagmata.svelte.ts";
  import { timezoneSetting } from "../../lib/time/stamp.svelte.ts";
  import {
    common_cancel,
    common_save,
    auth_couldnt_reach,
    account_tokens_copy,
    account_tokens_create,
    account_tokens_created,
    account_tokens_edit,
    account_tokens_effect,
    account_tokens_expires,
    account_tokens_expiry,
    account_tokens_expiry_days,
    account_tokens_intro,
    account_tokens_last_used,
    account_tokens_name,
    account_tokens_name_placeholder,
    account_tokens_name_required,
    account_tokens_never_used,
    account_tokens_none,
    account_tokens_perms,
    account_tokens_perms_required,
    account_tokens_revoke,
    account_tokens_revoked,
    account_tokens_secret_desc,
    account_tokens_secret_title,
  } from "../../paraglide/messages.js";

  const tokens = $derived(archeionSession.tokens);
  const expiryChoices = [30, 90, 365];

  let name = $state("");
  let expiry = $state(90);
  let createPerms = $state<string[]>([...PAT_DEFAULT_PERMS]);
  let creating = $state(false);
  // The one plaintext, held until the owner dismisses it.
  let issued = $state<{ id: string; plaintext: string } | null>(null);
  let editingId = $state<string | null>(null);
  let editPerms = $state<string[]>([]);
  let confirmingRevoke = $state<string | null>(null);
  let busyId = $state<string | null>(null);
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

  async function create(): Promise<void> {
    if (creating) return;
    error = null;
    notice = null;
    if (!name.trim()) {
      error = account_tokens_name_required();
      return;
    }
    if (createPerms.length === 0) {
      error = account_tokens_perms_required();
      return;
    }
    creating = true;
    try {
      const result = await archeionSession.createToken({
        name: name.trim(),
        perms: createPerms,
        expires_in_days: expiry,
      });
      issued = { id: result.token.id, plaintext: result.plaintext };
      name = "";
      createPerms = [...PAT_DEFAULT_PERMS];
    } catch (e) {
      error = msgOf(e);
    } finally {
      creating = false;
    }
  }

  function startEdit(t: TokenView): void {
    editingId = t.id;
    // Every stored string, unknown ones included: opening the editor must not
    // drop a grant, and saving what the row already fails on stays an error
    // rather than a silent rescope.
    editPerms = [...t.perms];
    error = null;
    notice = null;
  }

  async function saveEdit(id: string): Promise<void> {
    if (busyId) return;
    if (editPerms.length === 0) {
      error = account_tokens_perms_required();
      return;
    }
    busyId = id;
    error = null;
    notice = null;
    try {
      await archeionSession.updateToken(id, { perms: editPerms });
      editingId = null;
      notice = account_tokens_effect();
    } catch (e) {
      error = msgOf(e);
    } finally {
      busyId = null;
    }
  }

  async function revoke(id: string): Promise<void> {
    if (busyId) return;
    busyId = id;
    error = null;
    notice = null;
    try {
      await archeionSession.revokeToken(id);
      confirmingRevoke = null;
    } catch (e) {
      error = msgOf(e);
    } finally {
      busyId = null;
    }
  }
</script>

<section class="space-y-3">
  <div class="card preset-tonal-surface p-4 space-y-3">
    <p class="text-xs opacity-60">{account_tokens_intro()}</p>
    <p class="text-xs opacity-60">{account_tokens_effect()}</p>

    {#if tokens.length === 0}
      <p class="text-sm opacity-60">{account_tokens_none()}</p>
    {:else}
      <ul class="space-y-3">
        {#each tokens as t (t.id)}
          {@const parts = splitPerms(t.perms)}
          <li
            class="space-y-2 border-t border-surface-500/20 pt-3 first:border-0 first:pt-0"
          >
            <div class="flex flex-wrap items-center gap-2 text-sm">
              <span class="font-medium break-all">{t.name}</span>
              {#if t.masked}
                <span class="font-mono text-xs opacity-60 break-all"
                  >{t.masked}</span
                >
              {/if}
              {#if t.revoked_at}
                <span class="badge preset-filled-error-500 text-xs"
                  >{account_tokens_revoked({
                    date: stamp(t.revoked_at),
                  })}</span
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

            {#if editingId === t.id}
              <div class="space-y-2">
                <PermChecklist
                  value={editPerms}
                  onChange={(next) => (editPerms = next)}
                  unknown={parts.unknown}
                  disabled={busyId === t.id}
                />
                <div class="flex flex-wrap gap-2">
                  <button
                    type="button"
                    class="btn btn-sm preset-filled-primary-500"
                    disabled={busyId === t.id}
                    onclick={() => saveEdit(t.id)}>{common_save()}</button
                  >
                  <button
                    type="button"
                    class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
                    disabled={busyId === t.id}
                    onclick={() => (editingId = null)}>{common_cancel()}</button
                  >
                </div>
              </div>
            {:else if !t.revoked_at}
              <div class="flex flex-wrap gap-2">
                <button
                  type="button"
                  class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
                  disabled={busyId === t.id}
                  onclick={() => startEdit(t)}>{account_tokens_edit()}</button
                >
                {#if confirmingRevoke === t.id}
                  <button
                    type="button"
                    class="btn btn-sm preset-filled-error-500"
                    disabled={busyId === t.id}
                    onclick={() => revoke(t.id)}
                    >{account_tokens_revoke()}</button
                  >
                  <button
                    type="button"
                    class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
                    disabled={busyId === t.id}
                    onclick={() => (confirmingRevoke = null)}
                    >{common_cancel()}</button
                  >
                {:else}
                  <button
                    type="button"
                    class="btn btn-sm preset-outlined-error-500 hover:preset-filled-error-500"
                    disabled={busyId === t.id}
                    onclick={() => (confirmingRevoke = t.id)}
                    >{account_tokens_revoke()}</button
                  >
                {/if}
              </div>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}

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

    <form
      class="space-y-3 border-t border-surface-500/20 pt-3"
      onsubmit={(ev) => {
        ev.preventDefault();
        create();
      }}
    >
      <p class="text-sm font-medium">{account_tokens_create()}</p>
      <div class="flex flex-col gap-2 sm:flex-row">
        <input
          class="input text-sm"
          type="text"
          maxlength="64"
          placeholder={account_tokens_name_placeholder()}
          aria-label={account_tokens_name()}
          bind:value={name}
          disabled={creating}
        />
        <select
          class="select text-sm shrink-0"
          aria-label={account_tokens_expiry()}
          bind:value={expiry}
          disabled={creating}
        >
          {#each expiryChoices as days (days)}
            <option value={days}
              >{account_tokens_expiry_days({ days: String(days) })}</option
            >
          {/each}
        </select>
      </div>
      <fieldset class="space-y-1">
        <legend class="text-xs opacity-60">{account_tokens_perms()}</legend>
        <PermChecklist
          value={createPerms}
          onChange={(next) => (createPerms = next)}
          disabled={creating}
        />
      </fieldset>
      <button
        type="submit"
        class="btn preset-filled-primary-500 text-sm"
        disabled={creating}>{creating ? "…" : account_tokens_create()}</button
      >
    </form>
  </div>
</section>
