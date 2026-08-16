<script lang="ts">
  // Self-service linked OAuth identities: list linked providers, link new ones,
  // unlink. Linking navigates away to the provider (the callback page completes
  // the ceremony and returns here); unlinking is a hard-delete that the agora
  // refuses with 409 when it would remove the account's last sign-in method
  // (the symmetric last-method guard with passkeys). Mirrors EmailManager: reads
  // the store directly, surfaces per-action errors inline.
  import { agoraSession } from "../../lib/session/agora.svelte";
  import { AgoraApiError } from "@kallipai/kallip-agora-client";
  import {
    settings_linked_accounts,
    settings_linked_intro,
    settings_linked_none,
    settings_unlink,
    settings_link_provider,
    settings_last_signin_error,
    auth_reauth_required,
  } from "../../paraglide/messages.js";

  const identities = $derived(agoraSession.externalIdentities);

  // Provider id -> label, for rendering the linked rows. A provider may have
  // been disabled (dropped from the registry) after the user linked it; fall
  // back to the id itself so the row still names it.
  const labelOf = $derived(
    new Map(agoraSession.oauthProviders.map((p) => [p.id, p.label])),
  );

  // Link buttons: one per configured provider the user has NOT yet linked. The
  // common case is one identity per provider; a second account of the same
  // provider is a rare edge and is not surfaced as a primary affordance.
  const linkable = $derived(
    agoraSession.oauthProviders.filter(
      (p) => !identities.some((i) => i.provider === p.id),
    ),
  );

  let busy = $state(false);
  let error = $state<string | null>(null);

  function msgOf(e: unknown): string {
    if (e instanceof AgoraApiError) {
      if (e.status === 409) {
        return settings_last_signin_error();
      }
      // The link begin is step-up gated; a 403 means the session's freshness
      // has expired. Surface a friendly prompt (the retry UI is a follow-up).
      if (e.status === 403) {
        return auth_reauth_required();
      }
    }
    return e instanceof Error ? e.message : String(e);
  }

  async function link(provider: string): Promise<void> {
    if (busy) return;
    busy = true;
    error = null;
    try {
      // Navigates away to the provider; the rest happens on the callback page.
      await agoraSession.linkProvider(provider);
    } catch (e) {
      error = msgOf(e);
    } finally {
      // On a normal success the page unloads (navigation), so this is invisible;
      // it only matters if the browser suppresses the top-level navigation
      // (sandboxed iframe, extension), in which case it unblocks retry. Mirrors
      // `unlink`.
      busy = false;
    }
  }

  async function unlink(id: string): Promise<void> {
    if (busy) return;
    busy = true;
    error = null;
    try {
      await agoraSession.unlinkExternalIdentity(id);
    } catch (e) {
      error = msgOf(e);
    } finally {
      busy = false;
    }
  }
</script>

<section class="space-y-3">
  <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
    {settings_linked_accounts()}
  </h2>

  <div class="card preset-tonal-surface p-4 space-y-3">
    <p class="text-xs opacity-60">
      {settings_linked_intro()}
    </p>

    {#if identities.length === 0}
      <p class="text-sm opacity-60">{settings_linked_none()}</p>
    {:else}
      <ul class="space-y-2">
        {#each identities as ident (ident.id)}
          <li class="flex items-center justify-between gap-2 text-sm">
            <span class="min-w-0">
              <span class="font-medium"
                >{labelOf.get(ident.provider) ?? ident.provider}</span
              >
              {#if ident.display_name}
                <span class="opacity-60 truncate"> - {ident.display_name}</span>
              {/if}
            </span>
            <button
              type="button"
              class="btn btn-sm preset-tonal-surface shrink-0"
              disabled={busy}
              onclick={() => unlink(ident.id)}>{settings_unlink()}</button
            >
          </li>
        {/each}
      </ul>
    {/if}

    {#if error}
      <p role="alert" class="text-xs text-error-500 dark:text-error-400">
        {error}
      </p>
    {/if}

    {#if linkable.length > 0}
      <div class="flex flex-wrap gap-2">
        {#each linkable as p (p.id)}
          <button
            type="button"
            class="btn btn-sm preset-tonal-surface"
            disabled={busy}
            onclick={() => link(p.id)}
            >{settings_link_provider({ provider: p.label })}</button
          >
        {/each}
      </div>
    {/if}
  </div>
</section>
