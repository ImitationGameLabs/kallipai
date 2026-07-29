// Post-sanitize guard for <a> tags inside rendered markdown. Assistant text is
// LLM-authored, so a plain click on a link must never yank the user out of the
// SPA. Only a plain primary click would navigate the current tab, so that alone
// is swallowed; every other activation -- Ctrl/Cmd/Shift+click, middle-click,
// and macOS Option+click (download) -- is a native "open elsewhere / download"
// gesture and is left to the browser. Keyboard Enter is the one case handled
// explicitly, since natively it too would navigate the current tab.
//
// Mirrors the post-sanitize DOM-surgery pattern in code-block.ts. Shared
// invariant with that file: DOMPurify runs DEFAULTS ONLY (see markdown.ts) --
// we never widen its config, and we only attach listeners / static attributes
// against the already-sanitized tree, never interpolating dynamic markup.

// Hover hint so the non-standard click behavior is discoverable.
const HINT_TITLE = "Ctrl/Cmd+click to open";

// Native Enter would navigate the current tab; open a new tab instead so
// keyboard users can follow links without leaving the SPA. Reads the raw
// attribute (not the resolved `.href`) so a sanitization regression that
// dropped the href cannot fall back to the page's own URL. Must stay
// synchronous -- going async loses transient activation and trips popup
// blockers. globalThis (not `window`) satisfies deno lint's no-window.
function openInNewTab(a: HTMLAnchorElement): void {
  const href = a.getAttribute("href");
  if (!href) return;
  globalThis.open(href, "_blank", "noopener,noreferrer");
}

// Decorate each link at mount with a discoverability tooltip and a sandboxed
// new-tab target. DOMPurify strips `target` but preserves `rel`, so both are
// set unconditionally: `target=_blank` as the no-JS fallback, and `rel` is
// forced to include noopener/noreferrer with any author-supplied `opener`
// stripped -- otherwise an LLM-authored rel="opener" would re-enable reverse
// tabnabbing on the native open path. The {#key} remount always starts from a
// fresh tree, so there is no idempotency concern.
function decorateLink(a: HTMLAnchorElement): void {
  if (!a.getAttribute("title")) a.setAttribute("title", HINT_TITLE);
  a.setAttribute("target", "_blank");
  const rel = new Set(
    (a.getAttribute("rel") ?? "").trim().split(/\s+/).filter(Boolean),
  );
  rel.delete("opener");
  rel.add("noopener");
  rel.add("noreferrer");
  a.setAttribute("rel", [...rel].join(" "));
}

// Resolve the anchor for a delegated event. Works for clicks on nested
// children of <a> (e.g. <a><code>text</code></a>) because closest() walks up.
function anchorFromEvent(event: Event): HTMLAnchorElement | null {
  return (
    (event.target as HTMLElement | null)?.closest<HTMLAnchorElement>(
      "a[href]",
    ) ?? null
  );
}

// A plain primary click would navigate the current tab; it is the only
// activation we must block. Every modifier (plus middle-click and Option-click)
// is a distinct native "open elsewhere / download" gesture, so all of them
// must fall through to the browser.
function isPlainPrimaryClick(event: MouseEvent): boolean {
  return (
    event.button === 0 &&
    !event.ctrlKey &&
    !event.metaKey &&
    !event.shiftKey &&
    !event.altKey
  );
}

// Svelte action: decorate every <a> under `node` at mount and route link
// activation through delegated listeners. <Markdown> remounts its subtree on
// each `source` change (see the {#key} block there), so mount-time decoration
// always covers the current links -- no observer needed.
export function enhanceLinks(node: HTMLElement): {
  destroy: () => void;
} {
  node.querySelectorAll<HTMLAnchorElement>("a[href]").forEach(decorateLink);

  const onClick = (event: MouseEvent): void => {
    const a = anchorFromEvent(event);
    if (!a) return;
    if (isPlainPrimaryClick(event)) event.preventDefault();
  };

  const onKeydown = (event: KeyboardEvent): void => {
    if (event.key !== "Enter") return;
    const a = anchorFromEvent(event);
    if (!a) return;
    event.preventDefault();
    openInNewTab(a);
  };

  node.addEventListener("click", onClick);
  node.addEventListener("keydown", onKeydown);

  return {
    destroy(): void {
      node.removeEventListener("click", onClick);
      node.removeEventListener("keydown", onKeydown);
    },
  };
}
