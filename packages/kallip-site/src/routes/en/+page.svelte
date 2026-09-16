<script lang="ts">
  import { ArrowRight, CodeXml } from "@lucide/svelte";

  // The primary CTA stays in this page's locale tree; the GitHub button
  // is external and stays locale-independent.
  const docsHref = "/en/docs/";
</script>

<svelte:head>
  <title>KallipAI</title>
  <meta
    name="description"
    content="KallipAI is the open framework for building and running AI agents: large-scale multi-agent collaboration that is orderly, reliable, and trustworthy."
  />
  <meta property="og:title" content="KallipAI" />
  <meta
    property="og:description"
    content="KallipAI is the open framework for building and running AI agents: large-scale multi-agent collaboration that is orderly, reliable, and trustworthy."
  />
</svelte:head>

<!-- Hero: pure CSS gradient block, no bitmap assets. -->
<section
  class="bg-gradient-to-br from-primary-700 via-primary-600 to-tertiary-700"
>
  <div
    class="mx-auto flex min-h-[32rem] max-w-6xl flex-col items-center justify-center gap-6 px-4 py-24 text-center"
  >
    <h1 class="h1 max-w-2xl text-surface-50">
      The open framework for building and running AI agents
    </h1>
    <p class="max-w-xl text-lg text-surface-50">
      Large-scale multi-agent collaboration that is orderly, reliable, and
      trustworthy.
    </p>
    <div class="flex gap-4">
      <a
        href={docsHref}
        class="btn bg-white text-surface-950 shadow-lg hover:shadow-xl"
      >
        Get started
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
