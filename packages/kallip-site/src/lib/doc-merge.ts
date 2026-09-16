// Pure helpers behind the docs list assembly: slug-prefix stripping for the
// en/zh-cn glob trees and zh-cn override merging. Kept free of the docs
// module so both sides stay independently testable.

// Glob keys carry their tree root ("../../../../docs/en/...", and zh-cn
// likewise); strip the given root so both sides land in one slug space.
export function stripDocPrefix(key: string, root: string): string {
  return key.replace(root, "").replace(/\.md$/, "");
}

// zh-cn entries override the en entry of the same slug wholesale; en
// entries without a zh-cn counterpart fall through unchanged (the fallback
// renders the en document). zh-cn entries with no en counterpart are
// orphans: reported to the caller, never rendered.
export function mergeOverrides<T extends { slug: string }>(
  en: readonly T[],
  zh: readonly T[],
): { merged: T[]; orphans: string[] } {
  const bySlug = new Map(en.map((doc) => [doc.slug, doc]));
  const orphans: string[] = [];
  for (const doc of zh) {
    if (bySlug.has(doc.slug)) bySlug.set(doc.slug, doc);
    else orphans.push(doc.slug);
  }
  return { merged: [...bySlug.values()], orphans };
}
