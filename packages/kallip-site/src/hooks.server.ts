import type { Handle } from "@sveltejs/kit";

import { isLocaleZh } from "./lib/lang.ts";

// Physical-locale routing: the path prefix is the single source of the
// document language (/zh-cn/** -> zh-cn, everything else -> en). The
// %lang% placeholder in app.html is replaced per page at prerender time,
// so the emitted HTML carries the correct lang attribute without any
// client-side script.
export const handle: Handle = ({ event, resolve }) => {
  const lang = isLocaleZh(event.url.pathname) ? "zh-cn" : "en";
  return resolve(event, {
    transformPageChunk: ({ html }) => html.replace("%lang%", lang),
  });
};
