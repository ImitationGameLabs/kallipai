<script lang="ts">
  // Language picker: setLocale writes the PARAGLIDE_LOCALE cookie and (paraglide
  // default) reloads the page, so every component re-renders with the new
  // locale and <html lang> is re-synced by the boot script — no per-component
  // reactivity needed. A no-reload reactive switch (locale as Svelte state)
  // is deferred (a reactive switch would trade the simple cookie+reload
  // flow for locale-as-state plumbing through every consumer).
  import { setLocale, getLocale, locales } from "../paraglide/runtime.js";
  import { settings_language } from "../paraglide/messages.js";

  let current = $state(getLocale());

  function pick(locale: (typeof locales)[number]) {
    setLocale(locale);
  }
</script>

<select
  class="text-sm"
  aria-label={settings_language()}
  value={current}
  onchange={(e) => pick(e.currentTarget.value as (typeof locales)[number])}
>
  {#each locales as locale (locale)}
    <option value={locale}>
      {locale === "en" ? "English" : locale === "zh" ? "中文" : locale}
    </option>
  {/each}
</select>
