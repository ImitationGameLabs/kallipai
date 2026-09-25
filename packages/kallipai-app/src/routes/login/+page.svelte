<script lang="ts">
  import { page } from "$app/state";
  import { LoginPage } from "@kallipai/kallipai-ui";

  // Honor the `?next=` set by the auth gate so a deep link returns after login.
  const returnPath = $derived(
    new URLSearchParams(page.url.search).get("next") ?? undefined,
  );
  // Build-time deployment flag (same injection channel as the domain): a
  // local-platform build shows the operator-key branch, a cloud build never
  // renders it (two-way information hiding).
  const offlineLogin = import.meta.env.KALLIPAI_OFFLINE_LOGIN === "1";
</script>

<LoginPage {returnPath} {offlineLogin} />
