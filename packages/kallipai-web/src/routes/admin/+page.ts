import { redirect } from "@sveltejs/kit";
import { adminGatewayPath } from "@kallipai/kallipai-ui";

// The admin hub is a permanent redirect to the gateway console: one
// canonical URL per admin surface, with the hub path kept as a stable
// entry point for links and bookmarks.
export const load = () => {
  redirect(301, adminGatewayPath());
};
