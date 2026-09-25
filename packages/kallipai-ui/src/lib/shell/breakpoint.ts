// The single structural breakpoint (the Tailwind md value): one matchMedia
// query string shared by every viewport verdict, so the root route's
// load-time check, the AppShell mount fork, and in-page listeners read the
// same truth and can never drift onto different breakpoints.
export const desktopQuery = "(min-width: 48rem)";
