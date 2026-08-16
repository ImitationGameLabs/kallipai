<script lang="ts">
  // The username input + its format hint, shared by the passkey register form
  // and the OAuth-signup username step so the field markup + validation copy
  // cannot drift. The owning page binds `value` and derives submit-readiness
  // from `isValidUsername` (the server is the final authority either way).
  import { isValidUsername } from "../lib/username.ts";
  import {
    auth_username,
    auth_username_hint,
    auth_username_placeholder,
  } from "../paraglide/messages.js";

  let { value = $bindable("") }: { value?: string } = $props();
  const valid = $derived(isValidUsername(value));
</script>

<label class="block space-y-1">
  <span class="text-sm opacity-70">
    {auth_username()} <span class="text-error-500 dark:text-error-400">*</span>
  </span>
  <input
    class="input"
    autocomplete="username"
    placeholder={auth_username_placeholder()}
    bind:value
    required
  />
  {#if value.length > 0 && !valid}
    <span class="text-xs text-error-500 dark:text-error-400">
      {auth_username_hint()}
    </span>
  {/if}
</label>
