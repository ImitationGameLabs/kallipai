// Copy text to the clipboard: the async Clipboard API when available, a
// legacy execCommand fallback when it is not. Returns false only when
// every path failed, so callers can skip the "copied" feedback instead
// of lying about success.
export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    // The API is absent in non-secure contexts (any http origin but
    // localhost) and in some webviews, and rejects on permission denial
    // or lost focus; the legacy path still works in all of those.
  }
  return legacyCopy(text);
}

// Legacy copy: a briefly-mounted offscreen textarea plus the deprecated
// document.execCommand("copy"). Deprecated, but still the only path that
// works in non-secure contexts and webviews without navigator.clipboard
// (WebKitGTK, the tauri Linux webview, among them). Needs the transient
// user activation of a click -- the only context copyText is called from.
function legacyCopy(text: string): boolean {
  const area = document.createElement("textarea");
  area.value = text;
  // Offscreen but rendered (display:none defeats selection on some
  // engines) and readonly so touch keyboards stay down.
  area.setAttribute("readonly", "");
  area.style.position = "fixed";
  area.style.left = "-9999px";
  area.style.opacity = "0";
  // A modal dialog's focus trap listens for focusin in the capture phase
  // and refocuses the dialog inside area.focus() when focus lands outside
  // its container, so a body-mounted textarea ends up unfocused and
  // execCommand("copy") fails. Mounting inside the open dialog keeps the
  // focus contained; body remains the mount point everywhere else.
  const active = document.activeElement;
  const scope =
    (active instanceof Element
      ? active.closest("[role='dialog'], dialog")
      : null) ??
    document.querySelector("[role='dialog'], dialog") ??
    document.body;
  scope.appendChild(area);
  area.focus();
  area.select();
  let copied = false;
  try {
    copied = document.execCommand("copy");
  } catch {
    copied = false;
  }
  area.remove();
  return copied;
}
