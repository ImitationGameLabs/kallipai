import { assert, assertMatch, assertNotMatch } from "@std/assert";

// The deployment's instances answer has two owners no unit test can reach: the
// edge writes `/config.js` from `compose/site.caddy`, and each compose override
// hands the edge the switch that decides it. Drop either half and the page goes
// back to probing a service the stack does not run -- silently, with every
// package still green, because a stopped container's refusal reaches the
// console as a CORS failure. These pins hold the meaning of both halves rather
// than their layout. The override list is written out because the package's
// test grant is per file, not per directory: a directory grant would open
// `compose/.env` as well, and a third override joins this list.
const SITE = new URL("../../../../../compose/site.caddy", import.meta.url);
const OVERRIDES = [
  new URL("../../../../../compose/compose.dev.yaml", import.meta.url),
  new URL("../../../../../compose/compose.prod.yaml", import.meta.url),
];
const WEB_TYPES = new URL(
  "../../../../kallipai-web/src/app.d.ts",
  import.meta.url,
);

/** The one line that publishes the runtime config. */
function payload(source: string): string {
  const line = source
    .split("\n")
    .find((candidate) => candidate.includes("window.KALLIPAI_CONFIG"));
  assert(line !== undefined, "the edge no longer answers /config.js");
  return line;
}

/** A compose override's `caddy` service, from its key to the next service. */
function caddyService(source: string, file: string): string {
  const lines = source.split("\n");
  const start = lines.findIndex((line) => /^ {2}caddy:\s*$/.test(line));
  assert(start >= 0, `${file} no longer declares a caddy service`);
  const body = lines.slice(start + 1);
  const end = body.findIndex((line) =>
    /^ {0,2}[A-Za-z0-9_.-]+:\s*$/.test(line),
  );
  return (end < 0 ? body : body.slice(0, end)).join("\n");
}

Deno.test("the payload carries the declared key and the mapped answer", () => {
  const answer = payload(Deno.readTextFileSync(SITE));
  assertMatch(
    Deno.readTextFileSync(WEB_TYPES),
    /instancesServiceEnabled\?:/,
    "the shell no longer declares the key the payload must carry",
  );
  assertMatch(answer, /"instancesServiceEnabled":\{instances_enabled\}/);
  // A quoted answer reaches the page as a string, and the store reads only the
  // literal false as off: that is the stopped-service probe coming back.
  assertNotMatch(answer, /"instancesServiceEnabled"\s*:\s*"/);
});

Deno.test("the switch is mapped before the payload that reads it", () => {
  const source = Deno.readTextFileSync(SITE);
  const mapped = source.search(
    /map\s+\{env\.KALLIPAI_INSTANCES\}\s+\{instances_enabled\}\s*\{/,
  );
  assert(mapped >= 0, "nothing maps the switch into the placeholder");
  const answered = source.indexOf("respond @config `");
  assert(answered >= 0, "the edge no longer answers /config.js");
  assert(mapped < answered, "the map runs after the response that reads it");
  // Every branch produces a string, so the map has one output type; the value
  // itself must stay a lowercase JSON boolean, quoted or not.
  assertMatch(source, /^\s*1\s+"?true"?\s*$/m);
  assertMatch(source, /^\s*default\s+"?false"?\s*$/m);
});

Deno.test("every override hands the edge the switch, on by default", () => {
  for (const override of OVERRIDES) {
    const block = caddyService(
      Deno.readTextFileSync(override),
      override.pathname,
    );
    assertMatch(
      block,
      /^\s+KALLIPAI_INSTANCES:\s*\$\{KALLIPAI_INSTANCES:-1\}\s*$/m,
    );
  }
});
