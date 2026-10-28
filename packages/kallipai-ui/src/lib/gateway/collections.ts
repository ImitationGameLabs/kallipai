// The gateway collection rows carry their member set names, so
// narrowing a flat set list down to one collection's members is a
// membership filter over names. A set name alone is not an identity —
// the same name may sit in another collection — so the row's
// collection must match too. Both faces write this shape (the console's
// detail and placement picks, the caller's grouped sets); one helper
// keeps the membership semantics single-sourced.
export function memberSetsOf<
  T extends { name: string; collection_name: string },
>(
  sets: readonly T[],
  collection: { name: string; sets: readonly string[] },
): T[] {
  const member = new Set(collection.sets);
  return sets.filter(
    (s) => s.collection_name === collection.name && member.has(s.name),
  );
}
