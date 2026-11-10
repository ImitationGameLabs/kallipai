// How a source pin reads a component: the whole file for a shape that can
// live anywhere, or its code lines when the shape being pinned is code and
// a copy sitting in a comment must not count. Kept in one place so the
// next pin does not invent a third reading.

export function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

/** The file's code lines: a line whose first non-blank characters open a
 * comment (`//`, `/*`, `*`, `<!--`) drops out -- the shape of an edit made
 * while debugging and left behind. Three limits, all deliberate: a
 * trailing comment is not stripped, so a pinned shape copied to the end of
 * another line still matches; the inner lines of a block or markup comment
 * are kept unless they lead with `*`, so a block that was commented out
 * still reads as code unless its lines open with a marker; and a code line
 * that begins with `*` drops out too (a wrapped multiplication, a `*`
 * selector in `<style>`), which costs a red pin and never a green one.
 * Nothing here tracks string or regex state, so neither an apostrophe in
 * the markup nor a `/[//]/` literal can switch the filter off. */
export function codeLinesOf(text: string): string {
  return text
    .split("\n")
    .filter((line) => {
      const rest = line.trimStart();
      return !(
        rest.startsWith("//") ||
        rest.startsWith("/*") ||
        rest.startsWith("*") ||
        rest.startsWith("<!--")
      );
    })
    .join("\n");
}

export function codeLines(url: URL): string {
  return codeLinesOf(source(url));
}
