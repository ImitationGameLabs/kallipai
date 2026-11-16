// The recents stamps in the reader's display zone. Formatting is pure and
// lives in navTime.ts; this wrapper is where the configured zone is read,
// because a rune read belongs in the reactive render path (deno test cannot
// compile runes, and the pure module has to stay importable there).
import { timezoneSetting } from "../time/stamp.svelte.ts";
import { formatRecentTime } from "./navTime.ts";

/** `at` as a recents row stamp, in the tagma's configured zone. */
export function recentStamp(at: number, now: number = Date.now()): string {
  return formatRecentTime(at, now, timezoneSetting.value);
}
