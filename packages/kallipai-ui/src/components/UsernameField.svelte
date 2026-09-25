<script lang="ts">
  // The username input + its format hint + availability probe, shared by the
  // passkey register form and the OAuth-signup username step so the field
  // markup + validation copy cannot drift. The owning page binds `value`,
  // derives submit-readiness from `isValidUsername`, and passes `paused`
  // while its submit is in flight (the server is the final authority either
  // way). The availability probe (3s debounce, then the archeion endpoint)
  // renders a check icon when a handle is free and red copy below the field
  // when it is taken or reserved.
  import { CircleCheck } from "@lucide/svelte";
  import { isValidUsername } from "../lib/username.ts";
  import {
    USERNAME_AVAILABILITY_DEBOUNCE_MS,
    foldStatus,
    isStale,
    shouldProbe,
    type AvailabilityState,
  } from "../lib/username-availability.ts";
  import { archeionClientOrFail } from "../lib/session/archeion.svelte";
  import {
    auth_username,
    auth_username_hint,
    auth_username_placeholder,
    auth_username_reserved,
    auth_username_taken,
  } from "../paraglide/messages.js";

  let {
    value = $bindable(""),
    paused = false,
  }: { value?: string; paused?: boolean } = $props();
  const valid = $derived(isValidUsername(value));
  const normalized = $derived(value.trim().toLowerCase());

  let availability = $state<AvailabilityState>({ phase: "idle" });
  let latestProbe = 0;

  // Debounced availability probe. The effect tracks the normalized handle
  // and the pause flag; each run bumps the token so a response landing
  // after the next keystroke (or after unmount) is discarded as stale, and
  // the cleanup cancels the pending timer.
  $effect(() => {
    const handle = normalized;
    if (paused || !shouldProbe(handle)) {
      availability = { phase: "idle" };
      return;
    }
    const token = ++latestProbe;
    availability = { phase: "checking" };
    const timer = setTimeout(() => {
      try {
        archeionClientOrFail()
          .usernameAvailability(handle)
          .then((r) => {
            if (!isStale(token, latestProbe))
              availability = foldStatus(r.status);
          })
          .catch(() => {
            // Transport failure: show no verdict rather than a stale one;
            // the submit-time checks remain the authority.
            if (!isStale(token, latestProbe)) availability = { phase: "idle" };
          });
      } catch {
        // Client not initialized (the offline shell mounts no signup, but a
        // render-time edge must not crash the field).
        if (!isStale(token, latestProbe)) availability = { phase: "idle" };
      }
    }, USERNAME_AVAILABILITY_DEBOUNCE_MS);
    return () => clearTimeout(timer);
  });
</script>

<label class="block space-y-1">
  <span class="text-sm opacity-70">
    {auth_username()} <span class="text-error-500 dark:text-error-400">*</span>
  </span>
  <div class="flex items-center gap-2">
    <input
      class="input"
      autocomplete="username"
      placeholder={auth_username_placeholder()}
      bind:value
      required
    />
    {#if availability.phase === "done" && availability.status === "available"}
      <CircleCheck
        class="size-5 shrink-0 text-success-500 dark:text-success-400"
        aria-hidden="true"
      />
    {/if}
  </div>
  {#if value.length > 0 && !valid}
    <span class="text-xs text-error-500 dark:text-error-400">
      {auth_username_hint()}
    </span>
  {:else if availability.phase === "done" && availability.status !== "available"}
    <span class="text-xs text-error-500 dark:text-error-400">
      {availability.status === "taken"
        ? auth_username_taken()
        : auth_username_reserved()}
    </span>
  {/if}
</label>
