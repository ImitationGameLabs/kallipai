<script lang="ts">
  import SvelteMarkdown, {
    buildUnsupportedHTML,
  } from "@humanspeak/svelte-markdown";
  import {
    createShikiHighlighter,
    getShikiHighlighter,
    setShikiHighlighter,
    ShikiCode,
  } from "@humanspeak/svelte-markdown/extensions/shiki";
  // Only the languages/themes imported here are bundled (Shiki is opt-in and
  // tree-shaken otherwise). The set covers what agent replies commonly emit in
  // this repo; unregistered languages fall back to a plain (still dark, framed)
  // code block rather than disappearing into the body text.
  import bash from "shiki/langs/bash.mjs";
  import css from "shiki/langs/css.mjs";
  import diff from "shiki/langs/diff.mjs";
  import html from "shiki/langs/html.mjs";
  import javascript from "shiki/langs/javascript.mjs";
  import json from "shiki/langs/json.mjs";
  import nix from "shiki/langs/nix.mjs";
  import python from "shiki/langs/python.mjs";
  import rust from "shiki/langs/rust.mjs";
  import sql from "shiki/langs/sql.mjs";
  import toml from "shiki/langs/toml.mjs";
  import typescript from "shiki/langs/typescript.mjs";
  import yaml from "shiki/langs/yaml.mjs";
  import githubDark from "shiki/themes/github-dark.mjs";
  import CopyButton from "./CopyButton.svelte";

  let { source }: { source: string } = $props();

  // Register the highlighter once (module singleton, idempotent across
  // re-imports/HMR). Synchronous -- pure-JS regex engine, safe at render time.
  if (!getShikiHighlighter()) {
    setShikiHighlighter(
      createShikiHighlighter({
        langs: [
          bash,
          css,
          diff,
          html,
          javascript,
          json,
          nix,
          python,
          rust,
          sql,
          toml,
          typescript,
          yaml,
        ],
        themes: [githubDark],
      }),
    );
  }

  // Disable raw HTML: agent markdown is parsed as markdown only. Stricter
  // than the old DOMPurify DEFAULTS path (which allowed a sanitized subset)
  // and removes that dependency. A suppressed tag renders as visible escaped
  // text rather than being silently dropped.
  const renderers = { html: buildUnsupportedHTML() };

  // target=_blank makes modifier-clicks and middle-click open a new tab
  // natively. A plain primary click would also follow target=_blank, so
  // onclick swallows it (preventDefault cancels even that new-tab navigation)
  // to keep links deliberate. Keyboard Enter synthesizes such a click, so it
  // is handled in onkeydown instead -- preventDefault there cancels the
  // synthesized click, and we open the new tab explicitly.
  function isPlainPrimaryClick(event: MouseEvent): boolean {
    return (
      event.button === 0 &&
      !event.ctrlKey &&
      !event.metaKey &&
      !event.shiftKey &&
      !event.altKey
    );
  }

  function openInNewTab(href: string): void {
    globalThis.open(href, "_blank", "noopener,noreferrer");
  }
</script>

<!--
  SvelteMarkdown (and ShikiCode) inject their elements as raw HTML, so those
  elements carry no Svelte scope hash and cannot be styled from a scoped
  component <style>. The per-element styling lives in styles/markdown.css
  (imported by each app's app.css), composed from Skeleton's themed tokens via
  @apply. See that file.
-->
<div class="markdown">
  <SvelteMarkdown {source} {renderers}>
    {#snippet link({ href, title, children })}
      {#if href}
        <a
          {href}
          title={title ?? "Ctrl/Cmd+click to open"}
          target="_blank"
          rel="noopener noreferrer"
          onclick={(event) => {
            if (isPlainPrimaryClick(event)) event.preventDefault();
          }}
          onkeydown={(event) => {
            if (event.key === "Enter") {
              event.preventDefault();
              openInNewTab(href);
            }
          }}
        >
          {@render children?.()}
        </a>
      {:else}
        <!-- href sanitized out (e.g. a javascript: URL); render as plain text -->
        {@render children?.()}
      {/if}
    {/snippet}

    {#snippet code({ lang, text })}
      <div class="group relative">
        <ShikiCode {lang} {text} />
        <CopyButton class="absolute top-1 right-1" getText={() => text} />
      </div>
    {/snippet}
  </SvelteMarkdown>
</div>
