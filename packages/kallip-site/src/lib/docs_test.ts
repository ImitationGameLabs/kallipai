// Segment-relative links are the only mechanism keeping repo-relative doc
// links inside whichever locale tree renders them; these fixtures pin the
// depth formula for the three link shapes, resolved under both trees.
import assert from "node:assert/strict";

import {
  internalLinkMessage,
  resolveTocAnchor,
  segmentRelativeHref,
  ungroupedSlugs,
} from "./doc-links.ts";

import { mergeOverrides, stripDocPrefix } from "./doc-merge.ts";

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

Deno.test("stripDocPrefix strips the en glob root", () => {
  assert.equal(
    stripDocPrefix(
      "../../../../docs/en/reference/auth.md",
      "../../../../docs/en/",
    ),
    "reference/auth",
  );
});

Deno.test("stripDocPrefix strips the zh-cn glob root", () => {
  assert.equal(
    stripDocPrefix(
      "../../../../docs/zh-cn/reference/auth.md",
      "../../../../docs/zh-cn/",
    ),
    "reference/auth",
  );
});

Deno.test(
  "mergeOverrides: zh-cn entry replaces the en entry of the same slug",
  () => {
    const en = [
      { slug: "architecture", n: 1 },
      { slug: "reference/auth", n: 2 },
    ];
    const zh = [{ slug: "reference/auth", n: 9 }];
    const { merged, orphans } = mergeOverrides(en, zh);
    assert.deepEqual(merged, [
      { slug: "architecture", n: 1 },
      {
        slug: "reference/auth",
        n: 9,
      },
    ]);
    assert.deepEqual(orphans, []);
  },
);

Deno.test(
  "mergeOverrides: untranslated slugs fall through to the en entry",
  () => {
    const en = [{ slug: "architecture", n: 1 }];
    const zh = [{ slug: "reference/auth", n: 9 }];
    const { merged } = mergeOverrides(en, zh);
    assert.deepEqual(merged, [{ slug: "architecture", n: 1 }]);
  },
);

Deno.test("mergeOverrides reports zh-cn orphans and keeps en ordering", () => {
  const en = [
    { slug: "z-doc", n: 1 },
    { slug: "a-doc", n: 2 },
  ];
  const zh = [{ slug: "ghost", n: 0 }];
  const { merged, orphans } = mergeOverrides(en, zh);
  assert.deepEqual(
    merged.map((d) => d.slug),
    ["z-doc", "a-doc"],
  );
  assert.deepEqual(orphans, ["ghost"]);
});

Deno.test(
  "resolveTocAnchor falls through to the suffix leg for a bare anchor",
  () => {
    const doc = {
      meta: {
        toc: {
          links: [{ id: "reference/auth-setup", text: "Setup", depth: 2 }],
        },
      },
    };
    assert.equal(resolveTocAnchor(doc, "setup"), "reference/auth-setup");
  },
);

Deno.test("resolveTocAnchor matches after collapsing hyphen runs", () => {
  const doc = {
    meta: {
      toc: {
        links: [{ id: "getting-started", text: "Getting started", depth: 1 }],
      },
    },
  };
  // GitHub keeps punctuation runs as multiple dashes in page anchors.
  assert.equal(resolveTocAnchor(doc, "getting--started"), "getting-started");
});

Deno.test("resolveTocAnchor falls back to a suffix match", () => {
  const doc = {
    meta: {
      toc: {
        links: [
          { id: "reference/auth-installation", text: "Installation", depth: 2 },
        ],
      },
    },
  };
  assert.equal(
    resolveTocAnchor(doc, "installation"),
    "reference/auth-installation",
  );
  assert.equal(resolveTocAnchor(doc, "missing"), "missing");
});

Deno.test("resolveTocAnchor passes anchors through without a TOC", () => {
  assert.equal(resolveTocAnchor(undefined, "keys"), "keys");
  assert.equal(resolveTocAnchor({ meta: {} }, "keys"), "keys");
});

Deno.test("resolveTocAnchor flattens nested children before matching", () => {
  const doc = {
    meta: {
      toc: {
        links: [
          {
            id: "reference/auth",
            text: "Auth",
            depth: 1,
            children: [
              {
                id: "reference/auth-token-types",
                text: "Token types",
                depth: 2,
              },
            ],
          },
        ],
      },
    },
  };
  assert.equal(
    resolveTocAnchor(doc, "token-types"),
    "reference/auth-token-types",
  );
});

Deno.test(
  "internalLinkMessage flags an internal doc kept as a repo path",
  () => {
    const slug = "development/setup";
    assert.equal(
      internalLinkMessage(slug, true),
      `[docs] internal doc linked from a published page: ${slug}`,
    );
    assert.equal(internalLinkMessage("reference/missing", false), undefined);
  },
);

Deno.test("ungroupedSlugs flags slugs outside the nav groups", () => {
  assert.deepEqual(
    ungroupedSlugs(
      ["architecture", "reference/auth", "stray/guide", "reference/env"],
      ["docs", "reference"],
    ),
    ["stray/guide"],
  );
  assert.deepEqual(ungroupedSlugs(["naming"], ["docs", "reference"]), []);
});
