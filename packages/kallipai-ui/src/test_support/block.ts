// One statement's body, for a structural pin: from the `at` a pin found its
// anchor at, up to the `});` that closes the statement. Every effect pinned
// this way closes with `});` and nests no callback of its own, so the first
// `});` after the anchor is the end of the block -- a `slice` window would
// instead take in whatever statement follows, and a pin reading a neighbour's
// line would pass for the wrong reason. If such a body ever grows a nested
// callback, this boundary ends early (a false red, never a false green): give
// that pin the boundary it needs instead of widening this one.
// No boundary at all is no block: the pin then fails on an empty body rather
// than reading the rest of the file, where a same-named call elsewhere could
// satisfy it.
export function blockAt(text: string, at: number): string {
  const end = text.indexOf("});", at);
  if (end === -1) return "";
  return text.slice(at, end + 3);
}
