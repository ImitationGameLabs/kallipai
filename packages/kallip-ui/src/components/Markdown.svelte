<script lang="ts">
  import { renderMarkdown } from "../lib/tools/markdown";

  let { source }: { source: string } = $props();

  // Render sanitized markdown HTML. No `.prose` (tailwindcss/typography ships
  // its own non-theme palette); instead style the elements via arbitrary
  // variants so the text color inherits Skeleton's theme tokens. Preflight
  // strips list markers, so those are re-added.
  const html = $derived(renderMarkdown(source));
</script>

<div
  class="text-sm leading-relaxed [&_p]:my-2 first:[&_p]:mt-0 last:[&_p]:mb-0 [&_ul]:list-disc [&_ol]:list-decimal [&_ul]:pl-5 [&_ol]:pl-5 [&_li]:my-0.5 [&_h1]:mt-3 [&_h1]:mb-1 [&_h1]:text-lg [&_h1]:font-semibold [&_h2]:mt-3 [&_h2]:mb-1 [&_h2]:font-semibold [&_h3]:font-semibold [&_a]:link [&_blockquote]:border-l-2 [&_blockquote]:border-surface-300 [&_blockquote]:pl-3 [&_blockquote]:opacity-80 [&_pre]:rounded-md [&_pre]:bg-surface-100 [&_pre]:p-2 [&_pre]:overflow-x-auto [&_pre]:font-mono [&_pre]:my-2 [&_code]:font-mono"
>
  {@html html}
</div>
