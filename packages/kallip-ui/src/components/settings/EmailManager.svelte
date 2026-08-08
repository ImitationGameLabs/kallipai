<script lang="ts">
  // Self-service email management: list / add / verify / make-primary / remove.
  // Email is an optional contact channel (login resolves by username). A newly
  // added address starts UNVERIFIED; with only the logging transport wired, the
  // verification token is emitted to the agora log -- paste it into the verify
  // field here (or follow the link once a real SMTP provider is configured).
  import { agoraClientOrFail, agoraSession } from "../../lib/session/agora.svelte";
  import type { EmailSummary } from "@kallipai/kallip-agora-client";
  import { isValidEmail } from "../../lib/email.ts";

  const emails = $derived(agoraSession.user?.emails ?? []);

  let newAddress = $state("");
  let token = $state("");
  let busy = $state(false);
  let error = $state<string | null>(null);
  let notice = $state<string | null>(null);

  const addressValid = $derived(isValidEmail(newAddress));
  const canAdd = $derived(addressValid && !busy);

  function msgOf(e: unknown): string {
    return e instanceof Error ? e.message : String(e);
  }

  async function refresh(): Promise<void> {
    // Narrow refresh of just the emails slice (one round-trip to /me/emails),
    // not a full whoami().
    await agoraSession.refreshEmails();
  }

  async function add(): Promise<void> {
    if (!canAdd) return;
    busy = true;
    error = null;
    notice = null;
    try {
      await agoraClientOrFail().addEmail({ address: newAddress.trim() });
      await refresh();
      newAddress = "";
      notice =
        "Verification token emitted to the agora log (paste it below) until SMTP is wired.";
    } catch (e) {
      error = msgOf(e);
    } finally {
      busy = false;
    }
  }

  async function verify(): Promise<void> {
    if (!token.trim() || busy) return;
    busy = true;
    error = null;
    notice = null;
    try {
      await agoraClientOrFail().verifyEmail({ token: token.trim() });
      await refresh();
      token = "";
      notice = "Email verified.";
    } catch (e) {
      error = msgOf(e);
    } finally {
      busy = false;
    }
  }

  async function makePrimary(e: EmailSummary): Promise<void> {
    if (busy) return;
    busy = true;
    error = null;
    notice = null;
    try {
      await agoraClientOrFail().makeEmailPrimary(e.id);
      await refresh();
    } catch (err) {
      error = msgOf(err);
    } finally {
      busy = false;
    }
  }

  async function remove(e: EmailSummary): Promise<void> {
    if (busy) return;
    busy = true;
    error = null;
    notice = null;
    try {
      await agoraClientOrFail().removeEmail(e.id);
      await refresh();
    } catch (err) {
      error = msgOf(err);
    } finally {
      busy = false;
    }
  }
</script>

<section class="space-y-3">
  <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">Email</h2>

  <div class="card preset-tonal-surface p-4 space-y-3">
    <p class="text-xs opacity-60">
      Optional contact channel. Not used for login. Link an address, then verify
      it to mark it primary.
    </p>

    {#if emails.length === 0}
      <p class="text-sm opacity-60">No email linked.</p>
    {:else}
      <ul class="space-y-2">
        {#each emails as e (e.id)}
          <li class="flex items-center justify-between gap-2 text-sm">
            <span class="min-w-0 truncate font-mono break-all">{e.address}</span
            >
            <span class="flex shrink-0 items-center gap-2">
              {#if e.is_primary}
                <span class="badge preset-filled-secondary-500">primary</span>
              {/if}
              {#if e.verified_at}
                <span class="text-xs opacity-60">verified</span>
              {:else}
                <span class="text-xs text-warning-500">unverified</span>
              {/if}
            </span>
            <span class="flex shrink-0 items-center gap-2">
              {#if !e.is_primary && e.verified_at}
                <button
                  type="button"
                  class="btn btn-sm preset-tonal-surface"
                  disabled={busy}
                  onclick={() => makePrimary(e)}>Make primary</button
                >
              {/if}
              <button
                type="button"
                class="btn btn-sm preset-tonal-surface"
                disabled={busy}
                onclick={() => remove(e)}>Remove</button
              >
            </span>
          </li>
        {/each}
      </ul>
    {/if}

    {#if error}
      <p role="alert" class="text-xs text-error-500">{error}</p>
    {/if}
    {#if notice}
      <p class="text-xs opacity-70">{notice}</p>
    {/if}

    <form
      class="flex gap-2"
      onsubmit={(ev) => {
        ev.preventDefault();
        add();
      }}
    >
      <input
        class="input"
        type="email"
        autocomplete="email"
        placeholder="you@example.com"
        bind:value={newAddress}
        disabled={busy}
      />
      <button
        type="submit"
        class="btn preset-filled-primary-500 shrink-0"
        disabled={!canAdd}>{busy ? "…" : "Add"}</button
      >
    </form>

    {#if newAddress.length > 0 && !addressValid}
      <p class="text-xs text-error-500">Enter a valid email address.</p>
    {/if}

    <details class="text-xs">
      <summary class="cursor-pointer opacity-60">Verify a token</summary>
      <form
        class="mt-2 flex gap-2"
        onsubmit={(ev) => {
          ev.preventDefault();
          verify();
        }}
      >
        <input
          class="input"
          type="text"
          placeholder="sk-email-…"
          bind:value={token}
          disabled={busy}
        />
        <button
          type="submit"
          class="btn preset-tonal-surface shrink-0"
          disabled={busy || !token.trim()}>Verify</button
        >
      </form>
    </details>
  </div>
</section>
