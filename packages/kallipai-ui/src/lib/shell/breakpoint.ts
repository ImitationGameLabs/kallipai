// The viewport forks: one matchMedia query string per layout decision. Every
// reader takes its string from here, so the root route's load-time check, the
// AppShell mount fork, the composer, the status pills, and the two-column
// pages can never drift onto different breakpoints.

// The shell's navigation fork (the Tailwind md value): below it the app wears
// the phone shell.
export const desktopQuery = "(min-width: 48rem)";

// The large viewport (the Tailwind lg value): the width where a page may split
// into columns or lay out a wider row. Two columns need more room than the
// navigation fork leaves, so below it the turn history keeps one scroller.
export const lgQuery = "(min-width: 64rem)";
