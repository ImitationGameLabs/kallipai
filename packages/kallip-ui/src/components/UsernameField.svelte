<script lang="ts">
  // The username input + its format hint, shared by the passkey register form
  // and the OAuth-signup username step so the field markup + validation copy
  // cannot drift. The owning page binds `value` and derives submit-readiness
  // from `isValidUsername` (the server is the final authority either way).
  import { isValidUsername } from "../lib/username.ts";

  let { value = $bindable("") }: { value?: string } = $props();
  const valid = $derived(isValidUsername(value));
</script>

<label class="block space-y-1">
  <span class="text-sm opacity-70">
    Username <span class="text-error-500">*</span>
  </span>
  <input
    class="input"
    autocomplete="username"
    placeholder="a-z, 0-9, -, 3-32 chars"
    bind:value
    required
  />
  {#if value.length > 0 && !valid}
    <span class="text-xs text-error-500">
      3-32 chars: a-z 0-9, single hyphens only (no leading/trailing/consecutive)
    </span>
  {/if}
</label>
