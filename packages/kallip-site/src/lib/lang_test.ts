// Pins the language-switch contract of mirrorHref: same path in the other
// locale, trailing slash always; the bare root resolves to /en/.
import assert from "node:assert/strict";
import { isLocaleEn, isLocaleZh, mirrorHref } from "./lang.ts";

Deno.test("mirrorHref jumps between locale segments", () => {
  assert.equal(mirrorHref("/"), "/en/");
  assert.equal(mirrorHref("/en/"), "/zh-cn/");
  assert.equal(mirrorHref("/en/about/"), "/zh-cn/about/");
  assert.equal(mirrorHref("/zh-cn"), "/en/");
  assert.equal(mirrorHref("/zh-cn/"), "/en/");
  assert.equal(mirrorHref("/zh-cn/about/"), "/en/about/");
});

Deno.test("isLocaleZh pins the strict segment match", () => {
  assert.equal(isLocaleZh("/"), false);
  assert.equal(isLocaleZh("/en/about/"), false);
  assert.equal(isLocaleZh("/zh-cn/about/"), true);
  assert.equal(isLocaleZh("/zh-cn-abc/"), false);
});

Deno.test("isLocaleEn pins the strict segment match", () => {
  assert.equal(isLocaleEn("/"), false);
  assert.equal(isLocaleEn("/zh-cn/about/"), false);
  assert.equal(isLocaleEn("/en"), true);
  assert.equal(isLocaleEn("/en/about/"), true);
  assert.equal(isLocaleEn("/en-abc/"), false);
});
