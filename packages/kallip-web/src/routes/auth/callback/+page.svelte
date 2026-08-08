<script lang="ts">
  import { page } from "$app/state";
  import { OAuthCallbackPage } from "@kallipai/kallip-ui";

  // The provider redirects back with ?code=...&state=...
  const search = $derived(new URLSearchParams(page.url.search));
  const code = $derived(search.get("code") ?? undefined);
  const oauthState = $derived(search.get("state") ?? undefined);
</script>

<OAuthCallbackPage {code} {oauthState} />
