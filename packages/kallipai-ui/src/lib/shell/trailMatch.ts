/**
 * The breadcrumb matching engine, dependency-free on purpose: no stores, no
 * i18n, no Svelte -- just patterns in, segments out. Keeping it pure is what
 * makes the matcher unit-testable (the route table in lib/shell/breadcrumbs.ts
 * pulls the session stores in through its resolvers, which a deno test cannot
 * load). Patterns match segment-for-segment (`:name` captures one segment),
 * so `/rooms/:id` never swallows `/rooms/:id/settings` -- the segment count
 * must match exactly.
 */

/** One trail segment: `href` makes it a link, and the current segment
 * (normally the tail) renders aria-current="page" instead of a link. The
 * table owns the type; Breadcrumbs.svelte re-exports it for its call sites. */
export type BreadcrumbSegment = {
  label: string;
  href?: string;
  current?: boolean;
};

/** Read-only wildcard captures from a matched pathname. */
export type TrailParams = Readonly<Record<string, string>>;

/** The params a resolver actually reads, named in the type. The matcher
 * fills every `:name` in the pattern; an entry's pattern and its resolver's
 * name list must agree (a rename in one shows up right here in the other). */
export type With<Names extends string> = TrailParams & { [K in Names]: string };

export interface TrailEntry {
  /** Pathname pattern; `:name` segments capture one path segment each. */
  pattern: string;
  resolve: (params: TrailParams) => BreadcrumbSegment[];
}

/** Declare a table entry. The helper exists for one reason: a resolver that
 * narrows its params (`With<"id">`) is not directly assignable to the
 * interface's wide `(params: TrailParams)` shape -- function params are
 * contravariant -- so the one honest cast lives here instead of at every
 * row. The matcher fills every `:name` the pattern declares, which is the
 * runtime invariant the cast rests on. */
export function entry<P extends string>(
  pattern: string,
  resolve: (params: With<P>) => BreadcrumbSegment[],
): TrailEntry {
  return {
    pattern,
    // The cast is the contravariance concession: the matcher fills every
    // :name the pattern declares, which is exactly the narrow shape the
    // resolver declared for itself.
    resolve: resolve as (params: TrailParams) => BreadcrumbSegment[],
  };
}

// Segment-capture decode: page params elsewhere in the app are decoded
// (SvelteKit hands pages decoded params), so the trail compares and renders
// the same values. A malformed escape falls back to the raw segment.
function decode(value: string): string {
  try {
    return decodeURIComponent(value);
  } catch {
    return value;
  }
}

/** Match a pathname against the route table; the first entry whose pattern
 * matches segment-for-segment wins. Returns null for untabled routes (the
 * shell renders no bar there). */
export function matchTrail(
  table: readonly TrailEntry[],
  pathname: string,
): BreadcrumbSegment[] | null {
  const parts = pathname.split("/").filter((p) => p !== "");
  for (const { pattern, resolve } of table) {
    const specs = pattern.split("/").filter((p) => p !== "");
    if (specs.length !== parts.length) continue;
    const params: Record<string, string> = {};
    let matched = true;
    for (let i = 0; i < specs.length; i++) {
      const spec = specs[i];
      const part = parts[i];
      if (spec === undefined || part === undefined) {
        matched = false;
        break;
      }
      if (spec.startsWith(":")) {
        params[spec.slice(1)] = decode(part);
      } else if (spec !== part) {
        matched = false;
        break;
      }
    }
    if (matched) return resolve(params);
  }
  return null;
}

/** The mobile back-row target derived from a matched trail: the deepest
 * linked segment is the parent -- the last chain link the desktop bar
 * renders as a link. A trail with no linked segment (a pure tail) marks a
 * destination, not a drill, and yields null so the caller keeps the bar.
 * Dependency-free like the matcher itself: route policy (which paths drill
 * and which keep the bar) lives with the wired table in breadcrumbs.ts. */
export function backFromTrail(
  segments: readonly BreadcrumbSegment[] | null,
): { href: string; label: string } | null {
  if (!segments) return null;
  for (let i = segments.length - 1; i >= 0; i--) {
    const seg = segments[i];
    if (seg?.href) return { href: seg.href, label: seg.label };
  }
  return null;
}
