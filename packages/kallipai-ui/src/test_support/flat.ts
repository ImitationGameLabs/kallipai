// Source with comments and formatting out of the way: a pin matches the shape
// of the code, so neither a line break nor a commented-out call may decide it.
// Block comments go first (`<!-- -->` and `/* */` may span lines), then line
// comments -- the guard leaves a `//` that follows a colon alone, so a URL
// stays content -- then the whitespace, and last the trailing comma a wrapped
// call gains: only a flattened object literal ends in `, }`, so folding the
// comma before the whitespace would leave every wrapped one alone.
export function flat(text: string): string {
  return text
    .replace(/<!--[\s\S]*?-->/gu, " ")
    .replace(/\/\*[\s\S]*?\*\//gu, " ")
    .replace(/(^|[^:])\/\/.*$/gmu, "$1 ")
    .replace(/\s+/gu, " ")
    .replaceAll(", }", " }");
}
