// Segment-relative links are the only mechanism keeping repo-relative doc
// links inside whichever locale tree renders them; these fixtures pin the
// depth formula for the three link shapes, resolved under both trees.
import assert from "node:assert/strict";

import {
  internalLinkMessage,
  normalizeSlug,
  resolveTocAnchor,
  segmentRelativeHref,
  ungroupedSlugs,
} from "./doc-links.ts";

import {
  docGroups,
  flattenGroups,
  navBranchIds,
  navIdByHref,
  toNavNodes,
} from "./doc-tree.ts";
import type { DocEntry } from "./doc-entry.ts";

import { stripDocPrefix } from "./doc-links.ts";

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
    const href = segmentRelativeHref(
      "configuration/services",
      "configuration",
      "",
    );
    assert.equal(href, "../../configuration/services/");
    assert.equal(
      target(href, "/en/docs/reference/auth/"),
      "/en/docs/configuration/services/",
    );
    assert.equal(
      target(href, "/zh-cn/docs/reference/auth/"),
      "/zh-cn/docs/configuration/services/",
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
      ["architecture", "reference/auth", "stray/guide", "configuration/daemon"],
      ["docs", "reference", "configuration"],
    ),
    ["stray/guide"],
  );
  assert.deepEqual(ungroupedSlugs(["naming"], ["docs", "reference"]), []);
});
// A minimal DocEntry fixture: only the fields the tree reads.
function entry(slug: string, order?: number, title = slug): DocEntry {
  return {
    slug,
    frontmatter: {
      title,
      description: "d",
      ...(order === undefined ? {} : { order }),
    },
    body: "",
    source: "",
  };
}

Deno.test("normalizeSlug: a directory index page is the directory", () => {
  assert.equal(normalizeSlug("deployment/nixos/index"), "deployment/nixos");
  assert.equal(normalizeSlug("deployment/index"), "deployment");
  assert.equal(normalizeSlug("index"), "");
  assert.equal(normalizeSlug("reference/auth"), "reference/auth");
});

Deno.test("docGroups: top-level order is the code array", () => {
  const groups = docGroups([
    entry("reference/auth"),
    entry("architecture"),
    entry("harness-design/context-management", 20),
    entry("deployment/nixos/minimal", 20),
    entry("configuration/services", 50),
  ]);
  assert.deepEqual(
    groups.map((group) => group.name),
    ["docs", "harness-design", "deployment", "configuration", "reference"],
  );
});

Deno.test("docGroups: subgroups order by their index page order value", () => {
  const groups = docGroups([
    entry("deployment/zzz/index", 40, "Zed"),
    entry("deployment", undefined, "Deployment"),
    entry("deployment/aaa", 10, "Aaa"),
    entry("deployment/aaa/guide", 20),
  ]);
  const deployment = groups.find((group) => group.name === "deployment")!;
  assert.deepEqual(
    deployment.groups.map((group) => group.name),
    ["deployment/aaa", "deployment/zzz"], // aaa's index carries order 10
  );
  assert.equal(deployment.title, "Deployment");
  assert.equal(deployment.index?.slug, "deployment");
});

Deno.test("docGroups: entries within a level sort by order then slug", () => {
  const groups = docGroups([
    entry("deployment/nixos/https", 40),
    entry("deployment/nixos/index", undefined, "NixOS"),
    entry("deployment/nixos/minimal", 20),
    entry("deployment/nixos/proxy", 30),
    entry("deployment/nixos/operations", 50),
  ]);
  const nixos = groups.find((group) => group.name === "deployment")!.groups[0];
  assert.equal(nixos.name, "deployment/nixos");
  assert.equal(nixos.title, "NixOS");
  assert.equal(nixos.index?.slug, "deployment/nixos");
  // The index page carries no order, so unordered entries sort after
  // all ordered ones; the tree contract test below shows how it lands
  // in the flat order.
  assert.deepEqual(
    nixos.entries.map((doc) => doc.slug),
    [
      "deployment/nixos/minimal",
      "deployment/nixos/proxy",
      "deployment/nixos/https",
      "deployment/nixos/operations",
      "deployment/nixos",
    ],
  );
});

Deno.test("flattenGroups: the flat order is the tree preorder", () => {
  const flat = flattenGroups(
    docGroups([
      entry("reference/auth", 20),
      entry("deployment/nixos/https", 40),
      entry("deployment/nixos", undefined, "NixOS"),
      entry("deployment/nixos/minimal", 20),
      entry("deployment", undefined, "Deployment"),
      entry("architecture"),
      entry("deployment/nixos/proxy", 30),
      entry("deployment/nixos/operations", 50),
    ]),
  );
  assert.deepEqual(
    flat.map((doc) => doc.slug),
    [
      // docs group
      "architecture",
      // deployment group: its own entries, then each subgroup's subtree
      "deployment",
      "deployment/nixos/minimal",
      "deployment/nixos/proxy",
      "deployment/nixos/https",
      "deployment/nixos/operations",
      "deployment/nixos",
      // reference group
      "reference/auth",
    ],
  );
});

Deno.test(
  "docGroups is idempotent over the flattened list (the sidebar re-trees docs/zhDocs)",
  () => {
    const fixture = [
      entry("reference/auth", 20),
      entry("deployment/nixos/https", 40),
      entry("deployment/nixos/index", undefined, "NixOS"),
      entry("deployment/nixos/minimal", 20),
      entry("deployment/index", undefined, "Deployment"),
      entry("architecture"),
      entry("deployment/nixos/proxy", 30),
      entry("deployment/nixos/operations", 50),
    ];
    const shape = (groups: ReturnType<typeof docGroups>) =>
      JSON.stringify(groups);
    const once = docGroups(fixture);
    const twice = docGroups(flattenGroups(once));
    assert.equal(shape(twice), shape(once));
  },
);

Deno.test("children: direct entries and subgroups interleave by order", () => {
  const groups = docGroups([
    entry("configuration/index", 10, "Configuration"),
    entry("configuration/tagma/index", 10, "Tagma"),
    entry(
      "configuration/tagma/environment-variables",
      15,
      "Environment variables",
    ),
    entry("configuration/tagma/llm", 20),
    entry("configuration/tagma/agent", 30),
    entry("configuration/tagma/service", 40),
    entry("configuration/services", 50),
    entry("configuration/daemon", 60),
  ]);
  const configuration = groups.find((group) => group.name === "configuration")!;
  const tagmaGroup = configuration.groups.find(
    (group) => group.name === "configuration/tagma",
  )!;
  // Direct entries and the subgroup share one interleaved sequence, each
  // ranked by its own (or its index page's) order value.
  assert.deepEqual(
    configuration.children.map((child) =>
      child.kind === "entry" ? child.doc.slug : child.node.name,
    ),
    [
      "configuration", // 10
      "configuration/tagma", // 10 (its index page)
      "configuration/services", // 50
      "configuration/daemon", // 60
    ],
  );
  // The group's own index page carries order 10 and the quick reference
  // 15, so the index leads and the quick reference precedes the pages.
  assert.deepEqual(
    tagmaGroup.entries.map((doc) => doc.slug),
    [
      "configuration/tagma",
      "configuration/tagma/environment-variables",
      "configuration/tagma/llm",
      "configuration/tagma/agent",
      "configuration/tagma/service",
    ],
  );
});

Deno.test("toNavNodes: groups become branches, entries linked leaves", () => {
  const groups = docGroups([
    entry("configuration/index", 10, "Configuration"),
    entry("configuration/tagma/index", 10, "Tagma"),
    entry("configuration/tagma/llm", 20, "LLM"),
    entry("configuration/services", 50, "Cron"),
  ]);
  const nodes = toNavNodes(
    groups,
    (group) => group.title || group.name,
    (slug) => `/en/docs/${slug}/`,
  );
  // The virtual docs group has no index page: its heading stays unlinked.
  // The configuration group has one: the heading becomes the link.
  assert.deepEqual(
    nodes.map((node) => [node.id, node.label, node.href]),
    [
      ["docs", "docs", undefined],
      ["configuration", "Configuration", "/en/docs/configuration/"],
    ],
  );
  const configuration = nodes[1];
  // The group's own index entry no longer appears among the children.
  assert.deepEqual(
    configuration.children!.map((child) => [
      child.id,
      child.label,
      child.href,
      child.children?.length ?? 0,
    ]),
    [
      ["configuration/tagma", "Tagma", "/en/docs/configuration/tagma/", 1],
      ["configuration/services", "Cron", "/en/docs/configuration/services/", 0],
    ],
  );
  const tagmaBranch = configuration.children![0];
  assert.deepEqual(
    tagmaBranch.children!.map((leaf) => leaf.id),
    ["configuration/tagma/llm"],
  );
  // The empty virtual docs group has no children, so it is not a branch.
  assert.deepEqual(navBranchIds(nodes), [
    "configuration",
    "configuration/tagma",
  ]);
  // Navigating to a group's index page selects the group heading itself.
  assert.equal(navIdByHref(nodes, "/en/docs/configuration/"), "configuration");
  assert.equal(
    navIdByHref(nodes, "/en/docs/configuration/tagma/llm/"),
    "configuration/tagma/llm",
  );
  assert.equal(navIdByHref(nodes, "/en/docs/missing/"), undefined);
});
