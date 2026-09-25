// The site is fully static: every route renders at build time, and URLs are
// directory-shaped (about/ not about) to match the static hosting layout.
export const prerender = true;
export const trailingSlash = "always";
