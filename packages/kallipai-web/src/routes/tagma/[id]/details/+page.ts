import { redirect } from "@sveltejs/kit";
import { tagmaDetailsSectionPath } from "@kallipai/kallipai-ui";

// The details hub is a permanent redirect to the overview section: one
// canonical URL per section, with the hub path kept as a stable entry point
// for links and bookmarks.
export const load = ({ params }: { params: { id: string } }) => {
  redirect(301, tagmaDetailsSectionPath(params.id ?? "", "overview"));
};
