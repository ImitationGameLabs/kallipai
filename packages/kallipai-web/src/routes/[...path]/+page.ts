import { error, redirect } from "@sveltejs/kit";
import { resolveRoute } from "@kallipai/kallipai-ui";

// Each host serves every page through this one route. The shared route table
// decides what renders; this load settles what must happen before anything
// mounts: a redirect, or a 404 for a path no row covers. The view resolves
// the same pathname again to pick the component.
export const load = ({ url }: { url: URL }) => {
  const resolved = resolveRoute(url.pathname, url.searchParams);
  if (resolved.kind === "redirect")
    redirect(resolved.status, resolved.location);
  if (resolved.kind === "missing") error(404, "Not Found");
};
