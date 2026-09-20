<script lang="ts">
  import { page } from "$app/state";
  import { Check, ChevronDown, Languages } from "@lucide/svelte";
  import { Menu } from "@skeletonlabs/skeleton-svelte";
  import { enHref, isLocaleZh, mirrorHref, zhHref } from "$lib/lang";
  import { headerMenus } from "$lib/header-menus";
  import { zhDocs } from "$lib/docs";

  // Single locale read drives both copy and internal link targets, so the
  // header renders correctly on every page of either segment.
  const isZh = $derived(isLocaleZh(page.url.pathname));

  // The zh mirror of a docs page exists only when the zh tree carries
  // the slug; untranslated pages keep the switch on the "#" placeholder
  // below, so prerender never crawls a dead internal path. The en tree
  // is complete, and the static pages exist under both locales.
  const mirrorExists = $derived.by(() => {
    const path = page.url.pathname;
    if (isLocaleZh(path) || !path.startsWith("/en/docs/")) return true;
    let slug = path.slice("/en/docs/".length);
    if (slug.endsWith("/")) slug = slug.slice(0, -1);
    return zhDocs.some((doc) => doc.slug === slug);
  });

  // Placeholder entries point at "#" until their sections land, so
  // prerender never crawls a dead internal path.
  const localeHref = $derived((path: string) =>
    isZh ? zhHref(path) : enHref(path),
  );
  // Dropdown disclosure state: one menu open at a time; null = all closed.
  let openMenu: string | null = $state(null);

  const copy = $derived(
    isZh
      ? {
          home: "首页",
          docs: "文档",
          news: "博客",
          about: "关于",
          nav: "主导航",
          switch: "中文",
          switchLabel: "Switch language",
        }
      : {
          home: "Home",
          docs: "Docs",
          news: "News",
          about: "About",
          nav: "Main navigation",
          switch: "English",
          switchLabel: "切换语言",
        },
  );
  const menus = $derived(headerMenus(isZh));
  const langs = [
    { value: "en", label: "English" },
    { value: "zh", label: "中文" },
  ] as const;
  // Locale trees are prerendered per locale, so a switch is a native
  // browser navigation: the target page ships its own static HTML.
  function switchTo(value: (typeof langs)[number]["value"]) {
    const current = isZh ? "zh" : "en";
    if (value === current) return;
    if (!mirrorExists) {
      window.location.assign(value === "zh" ? zhHref("/") : enHref("/"));
      return;
    }
    window.location.assign(mirrorHref(page.url.pathname));
  }
</script>

<svelte:window
  onkeydown={(e) => {
    if (e.key === "Escape" && openMenu !== null) {
      document.getElementById(`header-menu-${openMenu}`)?.focus();
      openMenu = null;
    }
  }}
/>
<header class="border-b border-surface-200-800">
  <div class="mx-auto flex h-14 max-w-6xl items-center gap-6 px-4">
    <a
      href={localeHref("/")}
      class="flex h-7 items-center"
      aria-label="KallipAI home"
    >
      <!-- Wordmark: Kallip + theme-primary AI (Brand composition, md size). -->
      <span class="text-xl font-bold tracking-tight">
        Kallip<span class="text-primary-500 dark:text-primary-400">AI</span>
      </span>
    </a>
    <nav aria-label={copy.nav} class="flex flex-1 items-center text-sm">
      <a
        href={localeHref("/")}
        class="flex-1 text-center hover:text-primary-500">{copy.home}</a
      >
      <a
        href={localeHref("/docs/introduction/")}
        class="flex-1 text-center hover:text-primary-500">{copy.docs}</a
      >
      {#each menus as menu (menu.id)}
        <div
          class="relative flex flex-1 justify-center"
          onfocusout={(e) => {
            if (!e.currentTarget.contains(e.relatedTarget as Node | null))
              openMenu = null;
          }}
        >
          <button
            type="button"
            id={`header-menu-${menu.id}`}
            class="flex items-center gap-1 hover:text-primary-500"
            aria-expanded={openMenu === menu.id}
            aria-controls={`header-menu-panel-${menu.id}`}
            onclick={() => (openMenu = openMenu === menu.id ? null : menu.id)}
          >
            {menu.label}
            <ChevronDown size={14} />
          </button>
          {#if openMenu === menu.id}
            <div
              id={`header-menu-panel-${menu.id}`}
              class="absolute left-1/2 -translate-x-1/2 top-full z-10 mt-2 w-44 rounded-container border border-surface-200-800 bg-surface-100-900 p-2 shadow-lg"
            >
              {#each menu.items as item (item.label)}
                <a
                  href={item.href}
                  class="block rounded px-3 py-1.5 text-sm hover:text-primary-500"
                  onclick={() => (openMenu = null)}
                >
                  {item.label}
                </a>
              {/each}
            </div>
          {/if}
        </div>
      {/each}
      <a href="#" class="flex-1 text-center hover:text-primary-500"
        >{copy.news}</a
      >
      <a
        href={localeHref("/about/")}
        class="flex-1 text-center hover:text-primary-500">{copy.about}</a
      >
    </nav>
    <Menu
      positioning={{ placement: "bottom-end" }}
      aria-label={copy.switchLabel}
    >
      <Menu.Trigger
        class="preset-outlined-surface-500 hover:preset-filled-surface-500 inline-flex items-center gap-1.5 rounded px-2 py-1 text-sm"
        aria-label={copy.switchLabel}
      >
        <Languages size={16} />
        {copy.switch}
        <ChevronDown size={14} />
      </Menu.Trigger>
      <Menu.Positioner>
        <Menu.Content
          class="w-36 rounded-container border border-surface-200-800 bg-surface-100-900 p-1 text-sm shadow-lg"
        >
          {#each langs as lang (lang.value)}
            <Menu.OptionItem
              type="radio"
              value={lang.value}
              checked={lang.value === (isZh ? "zh" : "en")}
              onCheckedChange={(checked) => checked && switchTo(lang.value)}
            >
              <span
                class="flex items-center justify-between gap-2 rounded px-2 py-1.5 data-highlighted:bg-surface-200-800"
              >
                {lang.label}
                <Menu.ItemIndicator>
                  <Check size={14} />
                </Menu.ItemIndicator>
              </span>
            </Menu.OptionItem>
          {/each}
        </Menu.Content>
      </Menu.Positioner>
    </Menu>
  </div>
</header>
