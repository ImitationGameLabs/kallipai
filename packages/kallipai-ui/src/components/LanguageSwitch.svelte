<script lang="ts">
  // Language picker: setLocale writes the PARAGLIDE_LOCALE cookie and (paraglide
  // default) reloads the page, so every component re-renders with the new
  // locale and <html lang> is re-synced by the boot script — no per-component
  // reactivity needed. A no-reload reactive switch (locale as Svelte state)
  // is deferred (a reactive switch would trade the simple cookie+reload
  // flow for locale-as-state plumbing through every consumer).
  //
  // A segmented control, not a native <select>: the OS draws a select's
  // popup (Android: a system bottom sheet) outside the page where theme
  // CSS cannot reach it, so the popup stayed white in dark mode while the
  // closed control themed correctly. Two locales fit side by side with no
  // popup at all; if locales grow past a handful, revisit with a Listbox.
  import { SegmentedControl } from "@skeletonlabs/skeleton-svelte";
  import { setLocale, getLocale, locales } from "../paraglide/runtime.js";
  import { settings_language } from "../paraglide/messages.js";

  let current = $state(getLocale());

  // Controlled value that moves before setLocale reloads, so the indicator
  // tracks the pick even in the window before the reload lands.
  function onValueChange(details: { value: string | null }) {
    const next = details.value;
    if (next && next !== current) {
      current = next as (typeof locales)[number];
      setLocale(current);
    }
  }
</script>

<SegmentedControl
  value={current}
  {onValueChange}
  aria-label={settings_language()}
>
  <SegmentedControl.Control>
    <SegmentedControl.Indicator />
    {#each locales as locale (locale)}
      <SegmentedControl.Item value={locale}>
        <!-- The hidden input is what carries focus, keyboard arrowing, and the
             radio semantics; the Item label's `for` points at it. -->
        <SegmentedControl.ItemHiddenInput />
        <SegmentedControl.ItemText>
          {locale === "en" ? "English" : locale === "zh" ? "中文" : locale}
        </SegmentedControl.ItemText>
      </SegmentedControl.Item>
    {/each}
  </SegmentedControl.Control>
</SegmentedControl>
