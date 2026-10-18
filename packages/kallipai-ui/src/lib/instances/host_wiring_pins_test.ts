import { assert, assertEquals, assertMatch } from "@std/assert";

// The deployment declaration has two live surfaces no unit test can reach:
// the web shell must hand config.instancesServiceEnabled to initInstances,
// and both app.d.ts copies must declare the same config fields. The first
// is the declaration -- drop the argument and every deployment goes back
// to probing a service it may not serve, silently, with every package
// still green; the two d.ts files are mirrors of one runtime payload and
// diverge the moment one side gains a key the other lacks. Source pins are
// how this repo holds host adapters and twins together (twinRoutes, the
// gateway console pins).
const WEB_LAYOUT = new URL(
  "../../../../kallipai-web/src/routes/+layout.svelte",
  import.meta.url,
);
const WEB_TYPES = new URL(
  "../../../../kallipai-web/src/app.d.ts",
  import.meta.url,
);
const APP_TYPES = new URL(
  "../../../../kallipai-app/src/app.d.ts",
  import.meta.url,
);

/** The call's text, from the callee name through its closing `);`.
 * Comments go in one alternating pass -- a block comment or a line
 * comment, whichever starts first, the argument list included -- so a
 * leftover mention cannot stand in for the argument, and a `/*` inside
 * a line comment stays inside that comment. */
function callText(source: string, callee: string): string {
  const code = source.replace(/\/\*[\s\S]*?\*\/|(^|\s)\/\/.*$/gm, "$1");
  const at = code.indexOf(callee + "(");
  assert(at >= 0, `${callee} is not called at all`);
  const end = code.indexOf(");", at);
  assert(end > at, `${callee} call is not terminated`);
  return code.slice(at, end + 2);
}

Deno.test("the web shell hands the declaration to the instances client", () => {
  const layout = Deno.readTextFileSync(WEB_LAYOUT);
  assertMatch(
    callText(layout, "initInstances"),
    /instancesServiceEnabled:\s*config\.instancesServiceEnabled/,
  );
});

/** The config field names one app.d.ts declares (interface-field depth). */
function configFields(url: URL): string[] {
  const names: string[] = [];
  const source = Deno.readTextFileSync(url);
  for (const match of source.matchAll(/^\s{6}(\w+)\?:/gm)) {
    names.push(match[1]);
  }
  return names.sort();
}

Deno.test("both app type mirrors declare the same config fields", () => {
  const web = configFields(WEB_TYPES);
  const app = configFields(APP_TYPES);
  assert(
    web.includes("instancesServiceEnabled"),
    "web app.d.ts lost instancesServiceEnabled",
  );
  assertEquals(app, web, "the two app.d.ts mirrors diverged");
});
