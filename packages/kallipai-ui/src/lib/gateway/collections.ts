// The gateway collection rows carry their member set names, so
// narrowing a flat set list down to one collection's members is a
// membership filter over names. Both faces write this shape (the
// console's detail and placement picks, the caller's grouped sets);
// one helper keeps the membership semantics single-sourced.
export function memberSetsOf<T extends { name: string }>(
  sets: readonly T[],
  memberNames: readonly string[],
): T[] {
  const member = new Set(memberNames);
  return sets.filter((s) => member.has(s.name));
}
