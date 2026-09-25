// Which no-conversation channel states can never produce a channel on their
// own, for the manage page's placeholder. The manage routes never fire
// `ensureOpen`; auto-open only fires for online tagmas, so `absent` to a
// presence-resolved-offline peer and `unavailable` (a budget failure, nothing
// in flight) would otherwise show the "opening" copy forever -- same stall
// family the sidebar dot shed in links.ts `tagmaNavIndicator`.

import type { TagmaChannelState } from "../session/channels.svelte.ts";

/** True when `channel` can no longer open without a user retry. `absent`
 *  stalls only once presence resolved without the peer (the realtime resolve
 *  deadline bounds the wait); `pending` always keeps its in-flight meaning. */
export function manageChannelStalled(
  channel: TagmaChannelState,
  knownOffline: boolean,
): boolean {
  switch (channel.kind) {
    case "absent":
      // No channel and no auto-open will come: the peer is confirmed away.
      return knownOffline;
    case "unavailable":
      // The open budget holds a failure and nothing is in flight.
      return true;
    default:
      // pending (in flight) and the conversation-bearing kinds never stall.
      return false;
  }
}
