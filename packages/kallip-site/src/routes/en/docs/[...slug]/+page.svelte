<script lang="ts">
  import DocArticle from "$lib/components/DocArticle.svelte";
  import { docs } from "$lib/docs";

  let { data } = $props();
</script>

<svelte:head>
  <title>{data.seoTitle} · KallipAI</title>
  <meta name="description" content={data.seoDescription} />
</svelte:head>

<DocArticle
  document={data.document}
  tocLinks={data.tocLinks}
  doc={data.doc}
  prev={data.prev}
  next={data.next}
  entries={docs}
  base="/en"
  labels={{
    groups: {
      docs: "Docs",
      reference: "Reference",
    },
    docNav: "Docs navigation",
    onThisPage: "On this page",
    previous: "Previous",
    next: "Next",
  }}
/>
