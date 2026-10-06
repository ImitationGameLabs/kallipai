<script lang="ts">
  import { page } from "$app/state";
  import { GatewayCatalogPage } from "@kallipai/kallipai-ui";
</script>

<GatewayCatalogPage collection={page.params.name ?? ""} />
