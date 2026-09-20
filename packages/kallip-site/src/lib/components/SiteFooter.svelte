<script lang="ts">
  import { page } from "$app/state";
  import { enHref, isLocaleZh, zhHref } from "$lib/lang";

  // Same locale contract as the header; kept local so each component stays
  // independently renderable while lang.ts stays the single mirror source.
  const isZh = $derived(isLocaleZh(page.url.pathname));
  const localeHref = $derived((path: string) =>
    isZh ? zhHref(path) : enHref(path),
  );

  const year = new Date().getFullYear();
  const copy = $derived(
    isZh
      ? {
          nav: "页脚导航",
          docs: "文档",
          about: "关于",
          terms: "服务条款",
          privacy: "隐私政策",
          rights: "保留所有权利。",
        }
      : {
          nav: "Footer navigation",
          docs: "Docs",
          about: "About",
          terms: "Terms",
          privacy: "Privacy",
          rights: "All rights reserved.",
        },
  );
</script>

<footer class="border-t border-surface-200-800">
  <div class="mx-auto max-w-6xl px-4 py-10">
    <div class="flex h-6 items-center">
      <!-- Footer wordmark: Kallip + theme-primary AI (Brand composition). -->
      <span class="text-lg font-bold tracking-tight">
        Kallip<span class="text-primary-500 dark:text-primary-400">AI</span>
      </span>
    </div>
    <nav
      aria-label={copy.nav}
      class="mt-8 flex flex-wrap items-center gap-x-6 gap-y-2 text-sm"
    >
      <a href={localeHref("/docs/introduction/")} class="hover:text-primary-500"
        >{copy.docs}</a
      >
      <a
        href="https://github.com/kallipai"
        target="_blank"
        rel="noopener noreferrer"
        class="hover:text-primary-500">GitHub</a
      >
      <a href={localeHref("/about/")} class="hover:text-primary-500"
        >{copy.about}</a
      >
      <a href={localeHref("/terms/")} class="hover:text-primary-500"
        >{copy.terms}</a
      >
      <a href={localeHref("/privacy/")} class="hover:text-primary-500"
        >{copy.privacy}</a
      >
    </nav>
    <p
      class="mt-8 border-t border-surface-200-800 pt-6 text-sm text-surface-600-400"
    >
      © {year} KallipAI. {copy.rights}
    </p>
    <!-- ICP filing slot intentionally not rendered. -->
  </div>
</footer>
