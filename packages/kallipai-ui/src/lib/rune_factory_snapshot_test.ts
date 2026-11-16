// Factories in this package hand their rune cells out through getters. A
// shorthand -- `return { conv }` -- compiles to `return { conv: $.get(conv) }`,
// one read at factory time, so the caller holds whatever the module started
// with and the surface never repaints. Two factories did exactly that and no
// test in the package could see it: the value was frozen, never wrong.
//
// This scan walks every `.svelte.ts` module under src and reads the shape of its
// `return { ... }` literals: a cell named there as a bare entry is that
// snapshot. It guards the class, not a file list -- a factory added tomorrow is
// covered without anyone remembering this test. Test files are skipped, since
// the stub objects they build are not factories.
//
// It reads shapes, so it proves nothing about behaviour: a getter that returns
// the wrong cell passes here and fails a behaviour test. Its judgement is
// narrow too -- it covers a cell returned under its own name (`{ conv }` or
// `{ conv: conv }`), not a renamed read (`{ lines: visible }`) and not a spread.
// A green scan is not that class closed; it is that this one shape is gone.

import { assertEquals } from "@std/assert";

const SRC = new URL("../", import.meta.url);
const SOURCE = /\.svelte\.ts$/u;
const TEST_FILE = /_test\.ts$/u;
/** A cell declaration, shorthand or typed. */
const CELL =
  /\b(?:let|const)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]*)?=\s*\$(?:state|derived)(?:\.raw|\.by)?\b/gu;

/** The file's code with comments and string contents taken out, so neither a
 *  comment nor a message can look like an entry. */
function code(text: string): string {
  let out = "";
  for (let i = 0; i < text.length; i += 1) {
    const c = text[i];
    const next = text[i + 1];
    if (c === "/" && next === "/") {
      while (i < text.length && text[i] !== "\n") i += 1;
      out += " ";
    } else if (c === "/" && next === "*") {
      i += 2;
      while (i < text.length && !(text[i] === "*" && text[i + 1] === "/")) {
        i += 1;
      }
      i += 1;
      out += " ";
    } else if (c === '"' || c === "'" || c === "`") {
      i += 1;
      while (i < text.length && text[i] !== c) {
        if (text[i] === "\\") i += 1;
        i += 1;
      }
      out += '""';
    } else {
      out += c;
    }
  }
  return out;
}

/** The bodies of every `return { ... }` literal in the text. */
function returnBodies(text: string): string[] {
  const bodies: string[] = [];
  const at = /return\s*\{/gu;
  for (let match = at.exec(text); match !== null; match = at.exec(text)) {
    const start = match.index + match[0].length - 1;
    let depth = 0;
    for (let i = start; i < text.length; i += 1) {
      if (text[i] === "{") depth += 1;
      else if (text[i] === "}") {
        depth -= 1;
        if (depth === 0) {
          bodies.push(text.slice(start + 1, i));
          at.lastIndex = i;
          break;
        }
      }
    }
  }
  return bodies;
}

/** One object literal split at its own commas: nested calls stay whole. */
function entries(body: string): string[] {
  const out: string[] = [];
  let depth = 0;
  let current = "";
  for (const c of body) {
    if (c === "(" || c === "[" || c === "{") depth += 1;
    if (c === ")" || c === "]" || c === "}") depth -= 1;
    if (c === "," && depth === 0) {
      out.push(current.trim());
      current = "";
    } else {
      current += c;
    }
  }
  if (current.trim() !== "") out.push(current.trim());
  return out;
}

/** The cell entries a module's return literals hand out by value. */
function snapshots(text: string, name: string): string[] {
  const found = code(text);
  const cells = new Set<string>();
  for (const match of found.matchAll(CELL)) cells.add(match[1]);
  if (cells.size === 0) return [];
  const out: string[] = [];
  for (const body of returnBodies(found)) {
    for (const entry of entries(body)) {
      const bare = /^([A-Za-z_$][\w$]*)$/u.exec(entry)?.[1];
      const own = /^([A-Za-z_$][\w$]*)\s*:\s*\1$/u.exec(entry)?.[1];
      const cell = bare ?? own;
      if (cell !== undefined && cells.has(cell)) out.push(`${name}: ${cell}`);
    }
  }
  return out;
}

async function modules(dir: URL, prefix = ""): Promise<string[]> {
  const out: string[] = [];
  for await (const entry of Deno.readDir(dir)) {
    const name = prefix + entry.name;
    if (entry.isDirectory) {
      out.push(...(await modules(new URL(entry.name + "/", dir), name + "/")));
    } else if (SOURCE.test(entry.name) && !TEST_FILE.test(entry.name)) {
      out.push(name);
    }
  }
  return out.sort();
}

Deno.test("the scan flags the shape it is looking for", () => {
  const shim = "let conv = $state(undefined);\nlet lines = $derived(other);\n";
  assertEquals(
    snapshots(`${shim}function f() { return { conv, get lines() {} }; }`, "s"),
    ["s: conv"],
    "a bare cell is the snapshot; a getter beside it is not",
  );
  assertEquals(
    snapshots(`${shim}function f() { return { conv: conv }; }`, "s"),
    ["s: conv"],
    "reading it into a property is the same one read",
  );
  assertEquals(
    snapshots(
      `${shim}function f() { return { get conv() { return conv; } }; }`,
      "s",
    ),
    [],
    "a getter is the shape the factories are held to",
  );
});

Deno.test(
  "every factory hands its rune cells out through getters",
  async () => {
    const names = await modules(SRC);
    assertEquals(names.length > 20, true, `scanned ${names.length} modules`);
    const found: string[] = [];
    for (const name of names) {
      found.push(
        ...snapshots(await Deno.readTextFile(new URL(name, SRC)), name),
      );
    }
    assertEquals(
      found,
      [],
      "a bare rune cell in a return literal freezes at factory time",
    );
  },
);
