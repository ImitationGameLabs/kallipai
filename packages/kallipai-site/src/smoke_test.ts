// Smoke test: the static route tree must stay in place. These
// assertions pin the physical-locale contract (en under /en/, zh-cn
// under /zh-cn/, the bare root a detection shell) and the files that
// carry it.
import assert from "node:assert/strict";
Deno.test("route tree stays in place", async () => {
  for (const file of [
    "routes/+page.svelte",
    "routes/en/+page.svelte",
    "routes/en/about/+page.svelte",
    "routes/en/terms/+page.svelte",
    "routes/en/privacy/+page.svelte",
    "routes/zh-cn/+page.svelte",
    "routes/zh-cn/about/+page.svelte",
    "routes/zh-cn/terms/+page.svelte",
    "routes/zh-cn/privacy/+page.svelte",
    "routes/+layout.svelte",
    "routes/+layout.ts",
    "routes/en/docs/+page.ts",
    "routes/en/docs/[...slug]/+page.ts",
    "routes/en/docs/[...slug]/+page.svelte",
    "routes/zh-cn/docs/+page.ts",
    "routes/zh-cn/docs/[...slug]/+page.ts",
    "routes/zh-cn/docs/[...slug]/+page.svelte",
    "routes/md/docs/[...slug]/+server.ts",
    "routes/llms.txt/+server.ts",
    "routes/llms-full.txt/+server.ts",
    "routes/sitemap.xml/+server.ts",
    "routes/robots.txt/+server.ts",
  ]) {
    const info = await Deno.stat(new URL(file, import.meta.url));
    assert.ok(info.isFile, `${file} missing`);
  }
});
