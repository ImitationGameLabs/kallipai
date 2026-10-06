import { assert, assertEquals } from "@std/assert";

// Source-read pins for the required-field marker: the star is decorative
// (aria-hidden) and the required-ness semantics ride the control's
// aria-required attribute; every form label goes through the shared
// component instead of an ad-hoc star span. A sweep walks every package
// source for stray star spans, and the semantic pins cover both attribute
// forms (ARIA on labeled dialogs, native on UsernameField).
// The sweep roots mirror the test task's --allow-read grant: a
// pledge may only narrow it, so a new source dir joins both lists.

const MARK = new URL("./ReqMark.svelte", import.meta.url);
const PROFILE = new URL(
  "./manage/gateway/ProfileDialog.svelte",
  import.meta.url,
);
const CONNECT = new URL("../pages/ConnectPage.svelte", import.meta.url);
const USERNAME = new URL("./UsernameField.svelte", import.meta.url);
const PACKAGE = new URL("../../", import.meta.url);
const SOURCE_ROOTS = [
  "./src",
  "../kallipai-app/src",
  "../kallipai-direct/src",
  "../kallipai-site/src",
  "../kallipai-web/src",
].map((p) => new URL(`${p}/`, PACKAGE));

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

async function svelteSources(dir: URL): Promise<URL[]> {
  const found: URL[] = [];
  for await (const entry of Deno.readDir(dir)) {
    if (
      entry.isSymlink ||
      entry.name.startsWith(".") ||
      ["node_modules", "dist", "build", "coverage"].includes(entry.name)
    ) {
      continue;
    }
    if (entry.isDirectory) {
      found.push(...(await svelteSources(new URL(`${entry.name}/`, dir))));
    } else if (entry.name.endsWith(".svelte")) {
      found.push(new URL(entry.name, dir));
    }
  }
  return found;
}

Deno.test(
  "ReqMark is a decorative star carrying both error shades",
  { permissions: { read: [MARK] } },
  () => {
    const src = source(MARK);
    assert(
      src.includes('aria-hidden="true"'),
      "the star must be aria-hidden; assistive tech reads aria-required",
    );
    assert(
      src.includes("text-error-500 dark:text-error-400"),
      "the light and dark shades must live in the shared component",
    );
  },
);

Deno.test(
  "Form labels use ReqMark and the ad-hoc star spans are gone",
  { permissions: { read: [PROFILE, CONNECT] } },
  () => {
    assert(
      source(PROFILE).includes("<ReqMark />"),
      "the profile dialog labels must use the shared marker",
    );
    assert(
      !source(CONNECT).includes('dark:text-error-400">*</span'),
      "the connect page stars must be absorbed into ReqMark",
    );
  },
);

Deno.test(
  "The ad-hoc star span pattern has no residue outside ReqMark",
  { permissions: { read: SOURCE_ROOTS } },
  async () => {
    const files: URL[] = [];
    for (const root of SOURCE_ROOTS) {
      files.push(...(await svelteSources(root)));
    }
    const strays = files.filter(
      (url) =>
        url.href !== MARK.href && /<span[^>]*>\*<\/span>/.test(source(url)),
    );
    assertEquals(
      strays,
      [],
      "every required-mark star must come from the shared ReqMark component",
    );
  },
);

Deno.test(
  "Required semantics ride both attribute forms",
  { permissions: { read: [PROFILE, USERNAME] } },
  () => {
    assert(
      source(PROFILE).includes('aria-required="true"'),
      "the representative dialog label must carry the ARIA form",
    );
    assert(
      /^\s+required\s*$/m.test(source(USERNAME)),
      "the username input must carry the native required attribute",
    );
  },
);
