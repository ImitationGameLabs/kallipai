<script lang="ts">
  import { page } from "$app/state";
  import { PairPage } from "@kallipai/kallip-ui";

  // A pairing-code link (?code=XXXX-XXXX, e.g. from the QR) pre-fills the code.
  const code = $derived(
    new URLSearchParams(page.url.search).get("code") ?? undefined,
  );
</script>

<PairPage {code} />
