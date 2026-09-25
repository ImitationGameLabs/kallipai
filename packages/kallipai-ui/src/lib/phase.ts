// Shared dashboard section load phase. Generic across features (tagmata,
// rooms, ...) so a dashboard does not reach into a sibling feature module for
// it. A section is `loading` until its first successful fetch, `loaded` once it
// has data (even if a later refresh failed -- stale data stays visible), and
// `error` only before any data arrived.
export type SectionPhase = "loading" | "loaded" | "error";
