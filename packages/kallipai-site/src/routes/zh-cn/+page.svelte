<script lang="ts">
  import { ArrowRight, CodeXml } from "@lucide/svelte";

  // The primary CTA stays in this page's locale tree; the GitHub button
  // is external and stays locale-independent.
  const docsHref = "/zh-cn/docs/introduction/";
</script>

<svelte:head>
  <title>KallipAI</title>
  <meta
    name="description"
    content="KallipAI 是面向多智能体协作、长程任务和规模化设计的智能体运行框架（Agent Harness）。让大规模多智能体协作有序、可靠、可托付。"
  />
  <meta property="og:title" content="KallipAI" />
  <meta
    property="og:description"
    content="KallipAI 是面向多智能体协作、长程任务和规模化设计的智能体运行框架（Agent Harness）。让大规模多智能体协作有序、可靠、可托付。"
  />
</svelte:head>

<!-- Hero：纯 CSS 渐变色块，零位图资产。 -->
<section
  class="bg-gradient-to-br from-primary-700 via-primary-600 to-tertiary-700"
>
  <div
    class="mx-auto flex min-h-[32rem] max-w-6xl flex-col items-center justify-center gap-6 px-4 py-24 text-center"
  >
    <h1 class="h1 max-w-2xl text-surface-50">
      KallipAI 是面向多智能体协作、长程任务和规模化设计的智能体运行框架。
    </h1>
    <p class="max-w-xl text-lg text-surface-50">
      让大规模多智能体协作有序、可靠、可托付。
    </p>
    <div class="flex gap-4">
      <a
        href={docsHref}
        class="btn bg-white text-surface-950 shadow-lg hover:shadow-xl"
      >
        开始使用
        <ArrowRight size={16} />
      </a>
      <a
        href="https://github.com/kallipai"
        target="_blank"
        rel="noopener noreferrer"
        class="btn preset-outlined-primary-50"
      >
        <CodeXml size={16} />
        GitHub
      </a>
    </div>
  </div>
</section>
