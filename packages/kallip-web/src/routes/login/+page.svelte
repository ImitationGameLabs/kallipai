<script lang="ts">
  import { page } from "$app/state";
  import { LoginPage } from "@kallipai/kallip-ui";

  // Honor the `?next=` set by the auth gate so a deep link returns after login.
  const next = $derived(
    new URLSearchParams(page.url.search).get("next") ?? undefined,
  );
</script>

<LoginPage {next} />
