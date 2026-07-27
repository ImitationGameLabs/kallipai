// Post-sanitization DOM surgery that gives each rendered code block a copy
// button. `Markdown.svelte` runs marked + DOMPurify (DEFAULTS ONLY) and then
// injects {@html}; we cannot bake the button into the marked output because
// DOMPurify would strip any inline handler and the project forbids widening its
// config (see lib/tools/markdown.ts). So the button is injected here, against
// the already-sanitized DOM.
//
// INVARIANT: the two SVG strings below are the ONLY markup injected into the
// post-sanitization tree. They are module-level literals and must never
// interpolate dynamic values -- doing so would reopen the XSS surface that
// DOMPurify just closed.

import { copyText } from "../clipboard.ts";

const COPY_ICON_SVG =
  '<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="14" height="14" x="8" y="8" rx="2" ry="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/></svg>';

const CHECK_ICON_SVG =
  '<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg>';

// Wrapper carries `group` so the button can hover-reveal on desktop while
// staying faintly visible on touch (no `@media(hover:hover)` match).
const WRAPPER_CLASS = "group relative";
const BUTTON_CLASS =
  "absolute top-1 right-1 rounded p-1.5 text-surface-500 opacity-60 [@media(hover:hover)]:opacity-0 [@media(hover:hover)]:group-hover:opacity-100 focus-visible:opacity-100 hover:bg-surface-200 transition";

// Per-button flash timer so a rapid second copy resets the revert window
// instead of letting a stale timeout yank the check icon early.
const flashTimers = new WeakMap<
  HTMLButtonElement,
  ReturnType<typeof setTimeout>
>();

// Idempotently wrap a <pre> in a relative container and give it a copy button.
// The wrapper (not the <pre>) owns the button, so a horizontally-scrolling code
// block does not drag its copy button out of view.
export function decorateCodeBlock(pre: HTMLPreElement): void {
  if (pre.hasAttribute("data-copy-decorated")) return;
  pre.setAttribute("data-copy-decorated", "");

  const wrapper = document.createElement("div");
  wrapper.className = WRAPPER_CLASS;
  pre.parentElement?.insertBefore(wrapper, pre);
  wrapper.appendChild(pre);

  const button = document.createElement("button");
  button.type = "button";
  button.setAttribute("data-copy-button", "");
  button.setAttribute("aria-label", "Copy code");
  button.className = BUTTON_CLASS;
  button.innerHTML = COPY_ICON_SVG;
  wrapper.appendChild(button);
}

function flashCopied(button: HTMLButtonElement): void {
  button.innerHTML = CHECK_ICON_SVG;
  button.setAttribute("aria-label", "Copied");
  clearTimeout(flashTimers.get(button));
  const timer = setTimeout(() => {
    button.innerHTML = COPY_ICON_SVG;
    button.setAttribute("aria-label", "Copy code");
    flashTimers.delete(button);
  }, 1500);
  flashTimers.set(button, timer);
}

// Svelte action: decorate every <pre> under `node` at mount and route copies
// through one delegated listener. <Markdown> mounts once with finalized text in
// both consumers (TranscriptView finalizes before mounting; ChannelLine has no
// streaming flag), so mount-time decoration covers everything -- no observer.
export function enhanceCodeBlocks(node: HTMLElement): {
  destroy: () => void;
} {
  node.querySelectorAll("pre").forEach(decorateCodeBlock);

  const onClick = async (event: Event): Promise<void> => {
    const target = event.target as HTMLElement | null;
    const button = target?.closest<HTMLButtonElement>("[data-copy-button]");
    if (!button) return;
    // `code`-first, `pre`-fallback handles both fenced <pre><code> and bare
    // <pre>. textContent (never innerHTML) so nothing can execute.
    const pre = button.parentElement?.querySelector("pre");
    const code = pre?.querySelector("code");
    const text = code?.textContent ?? pre?.textContent ?? "";
    if (await copyText(text)) flashCopied(button);
  };
  node.addEventListener("click", onClick);

  return {
    destroy(): void {
      node.removeEventListener("click", onClick);
    },
  };
}
