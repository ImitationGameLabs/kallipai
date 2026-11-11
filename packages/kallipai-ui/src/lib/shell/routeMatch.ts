// Path matching for the shell-level route table. Each host app exposes one
// rest route (`[...path]`) and hands the pathname to this matcher, so the
// route list lives in one place (routeTable.ts) instead of one page file per
// route in every host tree.
//
// The rules mirror SvelteKit's own, because the same URLs must resolve to the
// same pages as before the route files moved:
//
// - the pathname is decoded with `split("%25").map(decodeURI).join("%25")`
//   (kit `decode_pathname`), so an escaped percent survives exactly one
//   round of decoding;
// - each captured param is decoded once with `decodeURIComponent` (kit
//   `decode_params`);
// - one trailing slash is tolerated, matching is case-sensitive, and an
//   empty segment never matches a param (kit builds `/seg/?$` from `+?`
//   captures).
//
// The trailing-slash tolerance is load-bearing, not a courtesy: kit
// normalises `url.pathname` only after the branch's loaders have run
// (`get_navigation_result_from_branch`, kit 2.69.2 `client.js:783`), so
// `load({ url })` sees the path as typed (`/chats/`). The route files this
// table replaces matched the same URL through kit's own `/?` suffix.
//
// Patterns are the route directory names this table replaces: "" for the
// root, "chats", "tagma/[id]/details/budget".
//
// Both hosts run with an empty `paths.base`. kit strips the base from the
// pathname before its own routing (`pathname.slice(base.length)` in
// `client.js`), and this matcher does not: a host that starts setting a base
// must strip it before calling matchPath, or every URL misses.

/** Captured params, keyed by the pattern's `[name]` segment. */
export type Params = Record<string, string>;

const PARAM_SEGMENT = /^\[(\w+)\]$/;

/** kit `decode_pathname`: decode URI escapes but leave `%25` for the param step. */
export function decodePathname(pathname: string): string {
  return pathname.split("%25").map(decodeURI).join("%25");
}

/**
 * Split a decoded pathname into segments: no leading empty, one trailing
 * slash dropped, root is the empty list.
 */
function segments(decoded: string): string[] {
  if (decoded === "/" || decoded === "") return [];
  const trimmed = decoded.endsWith("/") ? decoded.slice(0, -1) : decoded;
  const parts = trimmed.split("/");
  // The decoded pathname always starts with "/": drop that one empty part.
  if (parts[0] === "") parts.shift();
  return parts;
}

/**
 * Match a decoded pathname against a pattern; returns the params or null.
 * Callers pass the raw pathname to `matchPath`, which decodes first.
 */
export function matchDecoded(
  pattern: string,
  decodedPathname: string,
): Params | null {
  const patternSegments = pattern === "" ? [] : pattern.split("/");
  const pathSegments = segments(decodedPathname);
  if (patternSegments.length !== pathSegments.length) return null;

  const params: Params = {};
  for (let i = 0; i < patternSegments.length; i++) {
    const expected = patternSegments[i] ?? "";
    const actual = pathSegments[i] ?? "";
    if (actual === "" || expected === "") return null;
    const param = PARAM_SEGMENT.exec(expected);
    if (param === null) {
      if (expected !== actual) return null;
      continue;
    }
    const name = param[1];
    if (name === undefined) return null;
    // kit decodes params after the pathname-level decodeURI has run.
    params[name] = decodeURIComponent(actual);
  }
  return params;
}

/** Match a raw (URL) pathname against a pattern; returns the params or null. */
export function matchPath(pattern: string, pathname: string): Params | null {
  return matchDecoded(pattern, decodePathname(pathname));
}
