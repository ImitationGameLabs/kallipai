// Segment-relative links are the only mechanism keeping repo-relative doc
// links inside whichever locale tree renders them; these fixtures pin the
// depth formula for the three link shapes, resolved under both trees.
import assert from "node:assert/strict";

import { segmentRelativeHref } from "./doc-links.ts";

function target(href: string, from: string): string {
  return new URL(href, `https://site.test${from}`).pathname;
}

Deno.test(
  "segmentRelativeHref depth: root page into a nested reference doc",
  () => {
    const href = segmentRelativeHref("reference/auth", "", "");
    assert.equal(href, "../reference/auth/");
    assert.equal(
      target(href, "/en/docs/architecture/"),
      "/en/docs/reference/auth/",
    );
    assert.equal(
      target(href, "/zh-cn/docs/architecture/"),
      "/zh-cn/docs/reference/auth/",
    );
  },
);

Deno.test(
  "segmentRelativeHref depth: nested doc to a sibling nested doc",
  () => {
    const href = segmentRelativeHref("reference/env", "reference", "");
    assert.equal(href, "../../reference/env/");
    assert.equal(
      target(href, "/en/docs/reference/auth/"),
      "/en/docs/reference/env/",
    );
    assert.equal(
      target(href, "/zh-cn/docs/reference/auth/"),
      "/zh-cn/docs/reference/env/",
    );
  },
);

Deno.test("segmentRelativeHref depth: nested doc back to a root doc", () => {
  const href = segmentRelativeHref("development", "reference", "");
  assert.equal(href, "../../development/");
  assert.equal(
    target(href, "/en/docs/reference/auth/"),
    "/en/docs/development/",
  );
  assert.equal(
    target(href, "/zh-cn/docs/reference/auth/"),
    "/zh-cn/docs/development/",
  );
});

Deno.test("segmentRelativeHref carries a resolved anchor", () => {
  assert.equal(
    segmentRelativeHref("reference/auth", "", "keys"),
    "../reference/auth/#keys",
  );
});
