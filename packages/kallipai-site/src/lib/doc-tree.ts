// The docs nav tree: pure grouping, ordering, and flattening over a list
// of entries. No vite glob here — docs.ts feeds the entries in, tests feed
// fixtures, and the ordering contract (tree preorder = flat order) stays
// unit-testable without a build step.
import type { DocEntry } from "./doc-entry.ts";
import { normalizeSlug } from "./doc-links.ts";

// The nav tree. Top-level group order is fixed here (root docs, then
// harness-design/, then deployment/, then configuration/, then reference/); a new top-level group means touching this
// array, while a new subgroup under an existing group is a directory with
// an index.md and zero site-code change. Within a level, direct entries
// and subgroups interleave in one ordered sequence (`children`): an entry
// ranks by its own frontmatter `order`, a subgroup by its index page's
// (absent sorts last, ties by slug/name). The tree is the single ordering
// source: the flat docs list is its preorder traversal, and nothing
// downstream may re-sort.
export const groupOrder = [
  "docs",
  "harness-design",
  "deployment",
  "configuration",
  "reference",
];

// A node of the docs tree: one directory level. `name` is the path from
// the docs root ("docs" for the root documents, "deployment/nixos" for a
// subgroup); `title` is the index page's title ("" when the group has no
// index, callers fall back to their label maps); `entries` are the direct
// members of this level including the index; `groups` are the subgroups;
// `children` is the interleaved ordering sequence over both (the one
// renderers and preorder consume).
export interface DocGroupNode {
  name: string;
  title: string;
  index: DocEntry | undefined;
  entries: DocEntry[];
  groups: DocGroupNode[];
  children: DocChild[];
}

// One slot in a group's interleaved child sequence: a direct entry or a
// whole subgroup (whose order comes from its index page).
export type DocChild =
  | { kind: "entry"; doc: DocEntry }
  | { kind: "group"; node: DocGroupNode };

function compareEntries(a: DocEntry, b: DocEntry): number {
  return (
    (a.frontmatter.order ?? Number.POSITIVE_INFINITY) -
      (b.frontmatter.order ?? Number.POSITIVE_INFINITY) ||
    a.slug.localeCompare(b.slug)
  );
}

// One comparison for interleaved children: an entry ranks by its own
// frontmatter order, a subgroup by its index page's (absent sorts last);
// ties break by slug/name.
function compareChildren(a: DocChild, b: DocChild): number {
  const key = (c: DocChild) =>
    c.kind === "entry"
      ? {
          order: c.doc.frontmatter.order ?? Number.POSITIVE_INFINITY,
          name: c.doc.slug,
        }
      : {
          order: c.node.index?.frontmatter.order ?? Number.POSITIVE_INFINITY,
          name: c.node.name,
        };
  const ka = key(a);
  const kb = key(b);
  return ka.order - kb.order || ka.name.localeCompare(kb.name);
}

// Cluster one level of the tree: `list` is already the full set of entries
// under `name`; direct members stay, longer slugs recurse into subgroups.
function buildLevel(list: readonly DocEntry[], name: string): DocGroupNode {
  const prefix = name === "" ? "" : name + "/";
  const direct: DocEntry[] = [];
  const childNames: string[] = [];
  const byChild = new Map<string, DocEntry[]>();
  const put = (child: string, doc: DocEntry) => {
    if (!byChild.has(child)) {
      byChild.set(child, []);
      childNames.push(child);
    }
    byChild.get(child)!.push(doc);
  };
  // Clustering runs on the raw slugs: a directory's index page arrives as
  // "<group>/index", whose extra segment is what reveals the subgroup —
  // normalizing first would hide a one-file group behind a bare leaf.
  for (const doc of list) {
    const rest = doc.slug.slice(prefix.length);
    const slash = rest.indexOf("/");
    if (slash === -1) {
      direct.push(doc);
    } else {
      put(rest.slice(0, slash), doc);
    }
  }
  // A subgroup's index page can arrive in two shapes: the raw file shape
  // ("<group>/index", already clustered into the child above) or the
  // normalized directory shape ("<group>", a no-extra-segment member this
  // loop filed as a direct member). Reassign those to the child group they
  // name: a directory path is never a file, so the two cannot collide.
  for (const doc of [...direct]) {
    const rest = doc.slug.slice(prefix.length);
    if (childNames.includes(rest)) {
      direct.splice(direct.indexOf(doc), 1);
      put(rest, doc);
    }
  }
  direct.sort(compareEntries);
  // This level's own index page: the group's path, or the group path with
  // an /index tail (the raw file shape). Output slugs are normalized on
  // the way out — one slug per page, the directory itself for an index.
  const indexRaw = direct.find(
    (doc) => doc.slug === name || doc.slug === name + "/index",
  );
  // indexPage marks the one entry that IS this group's directory page;
  // every other member is a regular page even after normalization.
  const denorm = (doc: DocEntry): DocEntry => {
    const normalized = { ...doc, slug: normalizeSlug(doc.slug) };
    return doc === indexRaw ? { ...normalized, indexPage: true } : normalized;
  };
  const groups = childNames
    .map((child) => buildLevel(byChild.get(child)!, prefix + child))
    .sort(
      (a, b) =>
        (a.index?.frontmatter.order ?? Number.POSITIVE_INFINITY) -
          (b.index?.frontmatter.order ?? Number.POSITIVE_INFINITY) ||
        a.name.localeCompare(b.name),
    );
  const entriesOut = direct.map(denorm);
  const children: DocChild[] = [
    ...entriesOut.map((doc) => ({ kind: "entry" as const, doc })),
    ...groups.map((node) => ({ kind: "group" as const, node })),
  ];
  children.sort(compareChildren);
  return {
    name,
    title: indexRaw?.frontmatter.title ?? "",
    index: indexRaw ? denorm(indexRaw) : undefined,
    entries: entriesOut,
    groups,
    children,
  };
}

// The nav tree for one view: the virtual root's direct members become the
// "docs" group, one child group per top segment after that. Groups outside
// groupOrder sort last (they still render and warn — see ungroupedSlugs).
export function docGroups(list: readonly DocEntry[]): DocGroupNode[] {
  const root = buildLevel(list, "");
  // The virtual root's direct members become the "docs" group; the spread
  // must drop root's subgroups, or every subgroup would render twice (once
  // nested under docs, once as its own top-level group).
  const groups: DocGroupNode[] = [
    {
      ...root,
      name: "docs",
      groups: [],
      children: root.children.filter((child) => child.kind === "entry"),
    },
    ...root.groups,
  ];
  return groups.sort(
    (a, b) =>
      groupOrder.indexOf(a.name) - groupOrder.indexOf(b.name) ||
      a.name.localeCompare(b.name),
  );
}

// Preorder over the tree: each group's entries in order, then its subgroups
// in order. This flat list is the en canon the locale-independent surfaces
// (pager, sitemap, llms, the /md mirror) read.
function preorder(groups: readonly DocGroupNode[]): DocEntry[] {
  const out: DocEntry[] = [];
  const walk = (group: DocGroupNode) => {
    for (const child of group.children) {
      if (child.kind === "entry") out.push(child.doc);
      else walk(child.node);
    }
  };
  for (const group of groups) walk(group);
  return out;
}

// Preorder over a tree, re-exported shape for callers that flatten a view
// themselves.
export function flattenGroups(groups: readonly DocGroupNode[]): DocEntry[] {
  return preorder(groups);
}

// Sidebar TreeView payload: one node per entry (leaf, linked) or group
// (branch, expandable). Pure mapping so the component stays declarative
// and the shape stays testable without a build step.
export interface DocNavNode {
  id: string;
  label: string;
  href?: string;
  children?: DocNavNode[];
}

export function toNavNodes(
  groups: readonly DocGroupNode[],
  labelFor: (group: DocGroupNode) => string,
  hrefFor: (slug: string) => string,
): DocNavNode[] {
  // When a group has an index page, the group heading itself becomes the
  // link to it and the index entry drops out of the children: listing both
  // would render the same title twice, once as a heading and once as a
  // leaf. Groups without an index keep a plain, unlinked heading.
  const groupHref = (node: DocGroupNode): string | undefined =>
    node.index ? hrefFor(node.index.slug) : undefined;
  const isIndexEntry = (node: DocGroupNode, child: DocChild): boolean =>
    child.kind === "entry" &&
    node.index !== undefined &&
    child.doc.slug === node.index.slug;
  const toNode = (child: DocChild): DocNavNode =>
    child.kind === "entry"
      ? {
          id: child.doc.slug,
          label: child.doc.frontmatter.title,
          href: hrefFor(child.doc.slug),
        }
      : groupNode(child.node);
  const groupNode = (node: DocGroupNode): DocNavNode => ({
    id: node.name,
    label: labelFor(node),
    href: groupHref(node),
    children: node.children
      .filter((child) => !isIndexEntry(node, child))
      .map(toNode),
  });
  return groups.map(groupNode);
}

// Every branch id in the nav tree, for a fully expanded TreeView default.
export function navBranchIds(nodes: readonly DocNavNode[]): string[] {
  const out: string[] = [];
  const walk = (list: readonly DocNavNode[]) => {
    for (const node of list) {
      if (node.children && node.children.length > 0) {
        out.push(node.id);
        walk(node.children);
      }
    }
  };
  walk(nodes);
  return out;
}

// The nav id whose node points at href, for the TreeView selected state.
export function navIdByHref(
  nodes: readonly DocNavNode[],
  href: string,
): string | undefined {
  for (const node of nodes) {
    if (node.href === href) return node.id;
    if (node.children) {
      const found = navIdByHref(node.children, href);
      if (found !== undefined) return found;
    }
  }
  return undefined;
}
