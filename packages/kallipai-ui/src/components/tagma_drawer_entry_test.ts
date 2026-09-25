// Source-read pins for the drawer entry redesign (same rationale as
// agent_pills_test: presentation wiring a typecheck cannot see). The
// status line is the whole-row drawer target; the permanent corner
// button is gone.

import { assert } from "@std/assert";

const LINE = new URL("./TagmaStatusLine.svelte", import.meta.url);
const SHELL = new URL("../lib/shell/MobileShell.svelte", import.meta.url);
const APPSHELL = new URL("./AppShell.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the status line is the whole-row drawer button",
  { permissions: { read: [LINE] } },
  () => {
    const src = source(LINE);
    // The row is a real button (screen readers announce it as one), the
    // drawer-open label rides the row with the visible counts folded in,
    // and the expanded state is wired.
    assert(src.includes("<button"));
    assert(src.includes("aria-label={rowLabel}"));
    assert(src.includes("const rowLabel = $derived("));
    assert(src.includes("${status.subagentsActive}/${status.subagentsTotal}"));
    assert(src.includes('aria-haspopup="dialog"'));
    assert(src.includes("aria-expanded={expanded"));
    // Press feedback without button chrome: no border, no filled preset.
    assert(src.includes("active:preset-tonal-surface"));
    assert(!src.includes("border-"), "no border on the row target");
    assert(!src.includes("preset-filled"), "no filled preset on the row");
    // The chevron is a light hint, not a nested control: exactly one
    // button in the component (the row itself).
    assert((src.match(/<button/g) ?? []).length === 1);
    assert(src.includes("<ChevronDown"));
    assert(src.includes("opacity-50"));
  },
);

Deno.test(
  "the permanent corner menu button is gone from the mobile shell",
  { permissions: { read: [SHELL, APPSHELL] } },
  () => {
    const shell = source(SHELL);
    const app = source(APPSHELL);
    assert(!shell.includes("onMenu"), "the shell no longer takes onMenu");
    assert(!shell.includes("<Menu"), "the corner button is gone");
    assert(!app.includes("onMenu"), "AppShell no longer forwards onMenu");
    // The top row grid drops to three columns (back, centre, tail).
    assert(shell.includes("grid-cols-[auto_1fr_auto]"));
  },
);
