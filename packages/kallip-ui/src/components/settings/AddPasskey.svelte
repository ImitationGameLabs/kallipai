<script lang="ts">
  // The local "add" form: enroll another passkey on THIS browser's
  // authenticator (step-up-gated upstream). Form-only and prop-driven (the
  // chooser + trigger live in `AddDevice`); NOT cross-device — that is
  // `PairAnotherDevice`. Owns only the in-progress label text (the typed label
  // threads through the step-up re-auth, so it must not be dropped while busy).
  import type { PasskeyAddHint } from "../../lib/passkeys.svelte.ts";

  let {
    busy = false,
    hint = null,
    onAdd,
    onAdded,
  }: {
    // True while a ceremony is in flight (disables the controls).
    busy?: boolean;
    // The outcome of the last ceremony (success / error reason), rendered inline.
    hint?: PasskeyAddHint | null;
    // Commit the label; resolve `true` on success so the form clears, `false`
    // (or undefined) on failure so the user can retry without retyping -- the
    // store's driver preserves the label through the step-up re-auth server-side.
    // Pass `{ discoverable: true }` to enroll a resident (passwordless) credential.
    onAdd?: (
      label: string,
      opts?: { discoverable?: boolean },
    ) => Promise<boolean> | boolean | void;
    // Fired on a successful add; the owner folds the chooser back (the new card
    // in the list is the success feedback).
    onAdded?: () => void;
  } = $props();

  let label = $state("");
  // A discoverable credential lets the user sign in from the login page's
  // passkey autofill without typing their username.
  let discoverable = $state(false);

  async function submit() {
    const trimmed = label.trim();
    if (!trimmed || busy) return;
    const ok = (await onAdd?.(trimmed, discoverable ? { discoverable: true } : {})) ??
      false;
    if (ok) {
      label = "";
      discoverable = false;
      onAdded?.();
    }
  }
</script>

<div class="space-y-2">
  <div class="flex flex-wrap gap-2">
    <input
      class="input input-sm flex-1 min-w-32"
      placeholder="Passkey label (e.g. MacBook, YubiKey)"
      maxlength={64}
      bind:value={label}
      disabled={busy}
      onkeydown={(e) => e.key === "Enter" && submit()}
    />
    <button
      class="btn btn-sm preset-filled-primary-500"
      disabled={!label.trim() || busy}
      onclick={submit}
    >
      {busy ? "Adding..." : "Add passkey"}
    </button>
  </div>
  <label class="flex items-center gap-2 text-xs opacity-70 select-none">
    <input type="checkbox" bind:checked={discoverable} disabled={busy} />
    Enable passwordless sign-in (sign in from autofill, no username)
  </label>
  {#if hint}
    <div
      class="text-xs {hint.tone === 'ok'
        ? 'text-success-600 dark:text-success-500'
        : 'text-error-600 dark:text-error-500'}"
    >
      {hint.text}
    </div>
  {/if}
</div>
