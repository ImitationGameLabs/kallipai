// Copy buttons for rendered code blocks. The comark renderer emits bare
// `<pre>` elements and the prerender ships them as-is, so this action is
// the whole feature: it runs at hydration, wraps every `<pre>` under the
// host node, and mounts a button anchored to the wrapper's top-right
// corner. The wrapper (not the scrolling `<pre>`) carries the anchor, so
// horizontally scrolled code never carries the button away.
//
// Degradation is by absence: without JavaScript the prerendered pages
// have no button at all, which is the documented fallback. Clipboard
// access uses `navigator.clipboard` only — secure contexts only — and a
// failure (non-secure origin, denied permission) surfaces as a transient
// failed state on the button instead of a silent no-op.

export interface CopyCodeLabels {
  copy: string;
  copied: string;
  failed: string;
}

export function enhanceCodeBlocks(node: HTMLElement, labels: CopyCodeLabels) {
  const wraps: HTMLElement[] = [];
  // The host renders the @lucide/svelte Copy/Check icons into a hidden
  // template container; the action clones that markup into every button,
  // so the icon source stays the site's single lucide dependency.
  const iconOf = (name: string) =>
    node.querySelector(`[data-code-icon="${name}"]`)?.innerHTML ?? "";
  const copyIcon = iconOf("copy");
  const checkIcon = iconOf("check");

  const attach = (pre: HTMLPreElement) => {
    const wrap = document.createElement("div");
    wrap.className = "code-copy-wrap";
    pre.parentNode?.insertBefore(wrap, pre);
    wrap.appendChild(pre);

    const button = document.createElement("button");
    button.type = "button";
    button.className = "code-copy-btn";
    button.innerHTML = copyIcon;
    button.setAttribute("aria-label", labels.copy);
    button.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(pre.innerText);
        flash(
          button,
          checkIcon,
          labels.copied,
          "code-copy-ok",
          copyIcon,
          labels.copy,
        );
      } catch {
        flash(
          button,
          copyIcon,
          labels.failed,
          "code-copy-fail",
          copyIcon,
          labels.copy,
        );
      }
    });

    wrap.appendChild(button);
    wraps.push(wrap);
  };

  node.querySelectorAll("pre").forEach((pre) => attach(pre as HTMLPreElement));

  return {
    destroy() {
      wraps.forEach((wrap) => {
        const pre = wrap.querySelector("pre");
        if (pre) wrap.parentNode?.insertBefore(pre, wrap);
        wrap.remove();
      });
    },
  };
}

function flash(
  button: HTMLButtonElement,
  icon: string,
  label: string,
  stateClass: string,
  restoreIcon: string,
  restoreLabel: string,
) {
  button.innerHTML = icon;
  button.setAttribute("aria-label", label);
  button.classList.add(stateClass);
  setTimeout(() => {
    button.innerHTML = restoreIcon;
    button.setAttribute("aria-label", restoreLabel);
    button.classList.remove(stateClass);
  }, 1500);
}
