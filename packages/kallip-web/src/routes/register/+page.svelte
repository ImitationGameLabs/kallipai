<script lang="ts">
  import { page } from "$app/state";
  import { RegisterPage } from "@kallipai/kallip-ui";

  // An invite link (?invite=sk-invite-...) pre-fills the code.
  const invite = $derived(
    new URLSearchParams(page.url.search).get("invite") ?? undefined,
  );
</script>

<RegisterPage {invite} />
