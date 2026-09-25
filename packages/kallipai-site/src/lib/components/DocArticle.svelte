<script lang="ts">
  import { goto } from "$app/navigation";
  import { page } from "$app/state";
  import { MarkdownDocument } from "@comark/svelte";
  import { Check, Copy } from "@lucide/svelte";
  import {
    TreeView,
    createTreeViewCollection,
  } from "@skeletonlabs/skeleton-svelte";
  import type { TocLink } from "comark/plugins/toc";
  import type { DocNavNode } from "$lib/docs";
  import type { MarkdownDocument as ComarkDocument } from "comark";
  import { docGroups, navBranchIds, navIdByHref, toNavNodes } from "$lib/docs";
  import type { DocEntry, DocGroupNode } from "$lib/docs";
  import { enhanceCodeBlocks } from "$lib/actions/copy-code";

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
    // Which view (en canon or zh-cn overrides) feeds the sidebar and pager.
    entries: DocEntry[];
    // '' for the en segment, '/zh-cn' for the zh-cn segment.
    base: string;
    labels: {
      groups: Record<string, string>;
      docNav: string;
      onThisPage: string;
      previous: string;
      next: string;
      copyCode: string;
      copied: string;
      failed: string;
    };
  }

  let { document, tocLinks, doc, prev, next, base, labels, entries }: Props =
    $props();

  // Sidebar links keep the locale prefix so group navigation stays in-segment.
  const groups = $derived(docGroups(entries));
  const docHref = $derived((slug: string) => `${base}/docs/${slug}/`);
  const isActive = $derived(
    (slug: string) => page.url.pathname === docHref(slug),
  );
  // A group's label: the caller's map first (root groups have no index page),
  // then the group's own index title, then the raw name.
  const groupLabel = $derived(
    (group: DocGroupNode) =>
      (labels.groups[group.name] ?? group.title) || group.name,
  );
  // The TreeView payload: branches for groups, linked leaves for entries.
  const navNodes = $derived(toNavNodes(groups, groupLabel, docHref));
  const navCollection = $derived(
    createTreeViewCollection({
      rootNode: { id: "__root", label: "", children: navNodes },
      nodeToValue: (node: DocNavNode) => node.id,
      nodeToString: (node: DocNavNode) => node.label,
      nodeToChildren: (node: DocNavNode) => node.children ?? [],
    }),
  );
  // Fully expanded by default: this is a reference nav, and seeing every
  // page at once beats the click cost of unfolding it.
  const expanded = $derived(navBranchIds(navNodes));
  // The selected state is the component's single source of "current page"
  // visuals; aria-current stays on the link as a bare a11y marker.
  const currentPageId = $derived(navIdByHref(navNodes, page.url.pathname));
</script>

{#snippet navLevel(nodes: DocNavNode[], indexPath: number[])}
  {#each nodes as node, i (node.id)}
    {#if node.children && node.children.length > 0}
      {@const path = [...indexPath, i]}
      {@const nodeProps = { node, indexPath: path }}
      <TreeView.NodeProvider value={nodeProps}>
        <TreeView.Branch>
          <TreeView.BranchControl>
            <TreeView.BranchIndicator />
            <TreeView.BranchText>
              {#if node.href}
                <a
                  href={node.href}
                  aria-current={isActive(node.id) ? "page" : undefined}
                  onclick={(e) => {
                    const { href } = node;
                    if (href === undefined) return;
                    e.preventDefault();
                    e.stopPropagation();
                    goto(href);
                  }}
                >
                  {node.label}
                </a>
              {:else}
                {node.label}
              {/if}
            </TreeView.BranchText>
          </TreeView.BranchControl>
          <TreeView.BranchContent>
            {@render navLevel(node.children, path)}
          </TreeView.BranchContent>
        </TreeView.Branch>
      </TreeView.NodeProvider>
    {:else}
      {@const nodeProps = { node, indexPath: [...indexPath, i] }}
      <TreeView.NodeProvider value={nodeProps}>
        <TreeView.Item>
          <a
            href={node.href}
            aria-current={node.href && isActive(node.id) ? "page" : undefined}
          >
            {node.label}
          </a>
        </TreeView.Item>
      </TreeView.NodeProvider>
    {/if}
  {/each}
{/snippet}

{#snippet tocLevel(links: TocLink[], depth: number)}
  <ul class={depth === 2 ? "space-y-1 text-sm" : "mt-1 space-y-1 pl-3"}>
    {#each links as link (link.id)}
      <li>
        <a href="#{link.id}" class="text-surface-600-400 hover:text-primary-500"
          >{link.text}</a
        >
        {#if link.depth < 3 && link.children?.length}
          {@render tocLevel(link.children, link.depth)}
        {/if}
      </li>
    {/each}
  </ul>
{/snippet}

<div class="mx-auto flex w-full max-w-7xl gap-10 px-4 py-8">
  <aside class="hidden w-56 shrink-0 lg:block" aria-label={labels.docNav}>
    <TreeView
      collection={navCollection}
      defaultExpandedValue={expanded}
      selectedValue={currentPageId ? [currentPageId] : []}
      selectionMode="single"
      onSelectionChange={(details) => {
        const target = details.selectedNodes?.[0] as DocNavNode | undefined;
        if (target?.href) goto(target.href);
      }}
      class="text-sm"
    >
      <TreeView.Tree>
        {@render navLevel(navNodes, [])}
      </TreeView.Tree>
    </TreeView>
  </aside>

  {#key doc.slug}
    <article
      data-pagefind-body
      class="doc-body min-w-0 flex-1"
      use:enhanceCodeBlocks={{
        copy: labels.copyCode,
        copied: labels.copied,
        failed: labels.failed,
      }}
    >
      <h1 class="mb-8 text-3xl font-semibold">{doc.title}</h1>
      <MarkdownDocument value={document} />
      <div class="code-copy-icons" hidden aria-hidden="true">
        <span data-code-icon="copy"><Copy class="size-4" /></span>
        <span data-code-icon="check"><Check class="size-4" /></span>
      </div>
    </article>
  {/key}

  <aside class="hidden w-52 shrink-0 xl:block">
    {#if tocLinks.length > 0}
      <h2 class="mb-2 text-xs font-semibold tracking-wide uppercase">
        {labels.onThisPage}
      </h2>
      {@render tocLevel(tocLinks, 2)}
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
  /* Links: the comark renderer emits plain anchors, so body links
     would otherwise be indistinguishable from text. Color follows
     the theme's typo-anchor pair (rocket: primary-600 in light,
     primary-500 in dark), underlined by default; hover keeps the
     underline and switches to the site's usual primary-500 tone. */
  .doc-body :global(a) {
    color: light-dark(
      var(--typo-anchor--color-light),
      var(--typo-anchor--color-dark)
    );
    text-decoration-line: underline;
  }
  .doc-body :global(a:hover) {
    color: var(--color-primary-500);
  }
  .doc-body :global(pre) {
    margin-bottom: 1rem;
    overflow-x: auto;
    border-radius: 0.375rem;
    background: var(--color-surface-100-900);
    padding: 0.9rem;
    font-size: 0.875rem;
  }
  .doc-body :global(.code-copy-wrap) {
    position: relative;
  }
  /* Icon button (lucide-style inline SVG). Vertical anchor: the pre's
     padding-top (0.9rem) plus half the first line's height (1.7 x 0.875rem)
     minus half the button, so the icon centers on the first code line. */
  .doc-body :global(.code-copy-btn) {
    position: absolute;
    top: 0.9rem;
    right: 0.4rem;
    padding: 0.25rem;
    border: none;
    border-radius: 0.25rem;
    background: transparent;
    color: var(--color-surface-600-400);
    cursor: pointer;
    opacity: 0.6;
    transition:
      opacity 150ms,
      background-color 150ms;
  }
  .doc-body :global(.code-copy-btn svg) {
    display: block;
    width: 1rem;
    height: 1rem;
  }
  .doc-body :global(.code-copy-btn:hover) {
    background: var(--color-surface-200-800);
    opacity: 1;
  }
  .doc-body :global(.code-copy-btn:focus-visible) {
    opacity: 1;
    outline: 2px solid var(--color-primary-500);
    outline-offset: 1px;
  }
  @media (hover: hover) {
    .doc-body :global(.code-copy-btn) {
      opacity: 0;
    }
    .doc-body :global(.code-copy-wrap:hover .code-copy-btn),
    .doc-body :global(.code-copy-btn:focus-visible) {
      opacity: 1;
    }
  }
  .doc-body :global(.code-copy-ok) {
    color: var(--color-primary-500);
    opacity: 1;
  }
  .doc-body :global(.code-copy-fail) {
    color: var(--color-error-500);
    opacity: 1;
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
