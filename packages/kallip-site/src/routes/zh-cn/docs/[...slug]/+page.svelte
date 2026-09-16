<script lang="ts">
  import DocArticle from "$lib/components/DocArticle.svelte";

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
  base="/zh-cn"
  labels={{
    groups: {
      docs: "文档",
      reference: "参考",
    },
    docNav: "文档导航",
    onThisPage: "本页目录",
    previous: "上一篇",
    next: "下一篇",
  }}
/>
