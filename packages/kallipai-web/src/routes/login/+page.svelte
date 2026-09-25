<script lang="ts">
  import { page } from "$app/state";
  import { LoginPage } from "@kallipai/kallipai-ui";

  // Honor the `?next=` set by the auth gate so a deep link returns after login.
  const returnPath = $derived(
    new URLSearchParams(page.url.search).get("next") ?? undefined,
  );
  // Runtime deployment flag from /config.js (see app.d.ts): the factory
  // config.js ships offlineLogin = true (the self-hosted posture), and a
  // cloud-facing deployment hides the operator-key branch by setting it
  // to false explicitly — in runtimeConfig or in the file itself.
  const offlineLogin = window.KALLIPAI_CONFIG?.offlineLogin ?? true;
</script>

<LoginPage {returnPath} {offlineLogin} />
