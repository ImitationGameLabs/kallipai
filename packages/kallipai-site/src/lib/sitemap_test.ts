// Coverage rules for the sitemap builder: every human-facing page appears
// in the sitemap; docs pages pair with a zh mirror only when translated.
import assert from "node:assert/strict";

import { sitemapXml } from "./sitemap.ts";

const slugs = ["architecture", "reference/auth"];
// architecture stays untranslated here: the zh set carries one docs slug.
const zhSlugs = ["reference/auth"];
const xml = sitemapXml("https://example.test", slugs, zhSlugs);

Deno.test("sitemap covers the full expected page set", () => {
  // Explicit expectation list, not a count derived from the function:
  // a dropped or renamed route must go red by name, not by total.
  const expected = [
    "https://example.test/en/",
    "https://example.test/en/about/",
    "https://example.test/en/terms/",
    "https://example.test/en/privacy/",
    "https://example.test/en/docs/architecture/",
    "https://example.test/en/docs/reference/auth/",
    "https://example.test/zh-cn/",
    "https://example.test/zh-cn/about/",
    "https://example.test/zh-cn/terms/",
    "https://example.test/zh-cn/privacy/",
    "https://example.test/zh-cn/docs/reference/auth/",
  ];
  for (const url of expected) {
    assert.ok(xml.includes(`<loc>${url}</loc>`), `missing: ${url}`);
  }
  assert.equal(xml.split("<url>").length - 1, expected.length);
});

Deno.test("static pages pair with their zh-cn mirror and en x-default", () => {
  assert.ok(xml.includes("<loc>https://example.test/en/about/</loc>"));
  assert.ok(
    xml.includes('hreflang="zh-cn" href="https://example.test/zh-cn/about/"'),
  );
  assert.ok(
    xml.includes('hreflang="x-default" href="https://example.test/en/about/"'),
  );
  assert.ok(xml.includes("<loc>https://example.test/zh-cn/about/</loc>"));
});

Deno.test("docs pages pair only when the zh tree carries them", () => {
  assert.ok(
    xml.includes("<loc>https://example.test/en/docs/architecture/</loc>"),
  );
  assert.ok(
    xml.includes(
      'hreflang="zh-cn" href="https://example.test/zh-cn/docs/reference/auth/"',
    ),
  );
  // Untranslated: no zh loc and no zh-cn alternate, anywhere.
  assert.ok(!xml.includes("zh-cn/docs/architecture"));
});
Deno.test("hostile slug characters are XML-escaped", () => {
  const hostile = sitemapXml("https://example.test", ["a&b<c>"], []);
  assert.ok(
    hostile.includes(
      "<loc>https://example.test/en/docs/a&amp;b&lt;c&gt;/</loc>",
    ),
  );
  assert.ok(!hostile.includes("a&b<c>"));
});

Deno.test("transport formats stay out of the sitemap", () => {
  assert.ok(!xml.includes("/md/"));
  assert.ok(!xml.includes("llms"));
});
