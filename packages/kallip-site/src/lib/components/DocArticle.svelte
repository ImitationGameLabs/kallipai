<script lang="ts">
  import { page } from "$app/state";
  import { MarkdownDocument } from "@comark/svelte";
  import type { TocLink } from "comark/plugins/toc";
  import type { MarkdownDocument as ComarkDocument } from "comark";
  import { docGroups } from "$lib/docs";

  interface DocBrief {
    slug: string;
    title: string;
  }

  interface Props {
    document: ComarkDocument; // parsed AST, data-only and serializable
    tocLinks: TocLink[];
    doc: DocBrief;
    prev: DocBrief | null;
    next: DocBrief | null;
    // '' for the en segment, '/zh-cn' for the zh-cn segment.
    base: string;
    labels: {
      groups: Record<string, string>;
      docNav: string;
      onThisPage: string;
      previous: string;
      next: string;
    };
  }

  let { document, tocLinks, doc, prev, next, base, labels }: Props = $props();

  // Sidebar links keep the locale prefix so group navigation stays in-segment.
  const groups = $derived(docGroups());
  const docHref = $derived((slug: string) => `${base}/docs/${slug}/`);
  const isActive = $derived(
    (slug: string) => page.url.pathname === docHref(slug),
  );
</script>

<div class="mx-auto flex w-full max-w-7xl gap-10 px-4 py-8">
  <aside class="hidden w-56 shrink-0 lg:block" aria-label={labels.docNav}>
    {#each groups as group (group.name)}
      {#if group.entries.length > 0}
        <h2 class="mb-2 text-xs font-semibold tracking-wide uppercase">
          {labels.groups[group.name] ?? group.name}
        </h2>
        <ul class="mb-6 space-y-1 text-sm">
          {#each group.entries as entry (entry.slug)}
            <li>
              <a
                href={docHref(entry.slug)}
                class={`block rounded px-2 py-1 hover:text-primary-500 ${isActive(entry.slug) ? "bg-surface-100-900" : ""}`}
                aria-current={isActive(entry.slug) ? "page" : undefined}
              >
                {entry.frontmatter.title}
              </a>
            </li>
          {/each}
        </ul>
      {/if}
    {/each}
  </aside>

  <article data-pagefind-body class="doc-body min-w-0 flex-1">
    <h1>{doc.title}</h1>
    <MarkdownDocument value={document} />
  </article>

  <aside class="hidden w-52 shrink-0 xl:block">
    {#if tocLinks.length > 0}
      <h2 class="mb-2 text-xs font-semibold tracking-wide uppercase">
        {labels.onThisPage}
      </h2>
      <ul class="space-y-1 text-sm">
        {#each tocLinks as link (link.id)}
          <li>
            <a
              href="#{link.id}"
              class="text-surface-600-400 hover:text-primary-500">{link.text}</a
            >
          </li>
        {/each}
      </ul>
    {/if}
  </aside>
</div>

<nav class="mx-auto flex w-full max-w-7xl justify-between gap-4 px-4 pb-12">
  {#if prev}
    <a
      href={docHref(prev.slug)}
      class="text-sm hover:text-primary-500"
      data-pagefind-ignore
    >
      ← {labels.previous}: {prev.title}
    </a>
  {:else}
    <span></span>
  {/if}
  {#if next}
    <a
      href={docHref(next.slug)}
      class="text-sm hover:text-primary-500"
      data-pagefind-ignore
    >
      {labels.next}: {next.title} →
    </a>
  {/if}
</nav>

<style>
  /* Minimal document typography. The comark renderer emits unscoped markup,
     so these selectors are global; everything is scoped under .doc-body. */
  .doc-body :global(h2) {
    margin-top: 2rem;
    margin-bottom: 0.75rem;
    font-size: 1.35rem;
    font-weight: 600;
  }
  .doc-body :global(h3) {
    margin-top: 1.5rem;
    margin-bottom: 0.5rem;
    font-size: 1.15rem;
    font-weight: 600;
  }
  .doc-body :global(p) {
    margin-bottom: 0.9rem;
    line-height: 1.7;
  }
  .doc-body :global(ul),
  .doc-body :global(ol) {
    margin: 0 0 0.9rem 1.25rem;
    list-style: disc;
  }
  .doc-body :global(ol) {
    list-style: decimal;
  }
  .doc-body :global(pre) {
    margin-bottom: 1rem;
    overflow-x: auto;
    border-radius: 0.375rem;
    background: var(--color-surface-100-900);
    padding: 0.9rem;
    font-size: 0.875rem;
  }
  .doc-body :global(code) {
    font-size: 0.875em;
  }
  .doc-body :global(table) {
    width: 100%;
    margin-bottom: 1rem;
    border-collapse: collapse;
  }
  .doc-body :global(th),
  .doc-body :global(td) {
    border: 1px solid var(--color-surface-200-800);
    padding: 0.4rem 0.6rem;
    text-align: left;
  }
  .doc-body :global(blockquote) {
    margin: 0 0 1rem;
    border-left: 3px solid var(--color-primary-500);
    padding-left: 0.9rem;
  }
</style>
