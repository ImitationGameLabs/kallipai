// The docs index page is gone; the bare /docs/ path stays reachable as a
// permanent redirect into the introduction page.
import { redirect } from "@sveltejs/kit";
// $types is a SvelteKit-generated virtual module (no .ts on disk), so the
// sloppy-import rule can never see a real extension to accept.
// deno-lint-ignore no-sloppy-imports
import type { PageLoad } from "./$types";

export const load: PageLoad = () => {
  redirect(308, "/en/docs/introduction/");
};
