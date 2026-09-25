import { redirect } from "@sveltejs/kit";
import { desktopQuery } from "@kallipai/kallipai-ui";

// The panorama is a desktop-only page: one matchMedia verdict at load time
// sends small screens to the chats hub before anything mounts (no flash).
// Crossing the breakpoint after mount is PanoramaPage's own listener.
export const load = () => {
  if (!matchMedia(desktopQuery).matches) redirect(307, "/chats");
};
