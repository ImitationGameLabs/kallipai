// The files that draw a transcript bubble, taken from disk: a bubble is drawn
// through the shared row, so the file that imports `MessageRow` is the file
// that draws one -- as long as it imports it under that name. A surface
// written the house way announces itself here; one that renames the import
// slips past, and no formatter catches that. What this buys is the shape a
// hand-written list got wrong: the discussion surface carried its action row
// with no hover scope for a whole release, because no list named it.

import { flat } from "../../test_support/flat.ts";
import { source } from "./source_pins.ts";

export interface TranscriptSurface {
  /** The path under `src`, for a failure message. */
  label: string;
  /** The file itself. */
  url: URL;
}

/** Every `.svelte` file under `src` that imports the shared row. */
export function transcriptSurfaces(): TranscriptSurface[] {
  const found: TranscriptSurface[] = [];
  const walk = (dir: URL, prefix: string): void => {
    for (const entry of Deno.readDirSync(dir)) {
      const url = new URL(entry.name + (entry.isDirectory ? "/" : ""), dir);
      if (entry.isDirectory) {
        walk(url, `${prefix}${entry.name}/`);
        continue;
      }
      if (!entry.name.endsWith(".svelte")) continue;
      const src = flat(source(url));
      if (!/import MessageRow from ["']/u.test(src)) continue;
      found.push({ label: `${prefix}${entry.name}`, url });
    }
  };
  // The root is `src`, two steps up: this file sits in `lib/testing/`.
  walk(new URL("../../", import.meta.url), "");
  return found.sort((a, b) => a.label.localeCompare(b.label));
}
