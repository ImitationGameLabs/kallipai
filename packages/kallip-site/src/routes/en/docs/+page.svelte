<script lang="ts">
  import { docGroups, docs } from "$lib/docs";

  const groups = docGroups(docs);
</script>

<svelte:head>
  <title>Docs · KallipAI</title>
  <meta
    name="description"
    content="KallipAI documentation — architecture, guides, and reference."
  />
  <meta property="og:title" content="Docs · KallipAI" />
  <meta
    property="og:description"
    content="KallipAI documentation — architecture, guides, and reference."
  />
</svelte:head>

<div class="mx-auto w-full max-w-3xl px-4 py-12">
  <h1 class="mb-8 text-3xl font-semibold">Docs</h1>
  {#each groups as group (group.name)}
    {#if group.entries.length > 0}
      <section class="mb-8">
        <h2 class="mb-3 text-sm font-semibold tracking-wide uppercase">
          {{
            docs: "Docs",
            reference: "Reference",
          }[group.name] ?? group.name}
        </h2>
        <ul class="space-y-2">
          {#each group.entries as entry (entry.slug)}
            <li>
              <a href="/en/docs/{entry.slug}/" class="hover:text-primary-500">
                {entry.frontmatter.title}
              </a>
              <span class="block text-sm text-surface-500">
                {entry.frontmatter.description}
              </span>
            </li>
          {/each}
        </ul>
      </section>
    {/if}
  {/each}
</div>
