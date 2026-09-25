// Identity action shared by the two account surfaces: the sidebar footer
// dropdown (AccountMenu) and the /account hub page (the small-viewport
// replacement for the dropdown). Pure action logic -- every UI derivation
// (connection viewmodel, user-branch rendering) stays in the components so
// this module holds no presentation concerns. (The mode-switch actions
// left with the offline world, which moved to the kallipai-direct package.)
import { archeionSession } from "./archeion.svelte";
import { channelsStore } from "./channels.svelte";
import { roomsStore } from "./rooms.svelte";
import { roomConversationsStore } from "./roomConversations.svelte";
import { chatDraftsStore } from "./drafts.ts";
import { unreadStore } from "./unread.svelte.ts";

// Online: end the archeion session (destroys the cookie -- distinct from
import { statusCardStore } from "./statusCard.svelte.ts";
// switching, which keeps it). Drop open channels here; the realtime SSE that
// fed them is torn down separately by RootLayout's $effect when `user` flips
// to null (no 401 reconnect churn). The gate then sees user===null and
// redirects to /login (it owns the navigation), so no manual navigate here.
export async function logout() {
  channelsStore.reset();
  // Drop the per-user room registry + rendered room transcripts: they are
  // plaintext and keyed on the leaving user, so they must not linger into the
  // next session on a shared device.
  roomsStore.reset();
  roomConversationsStore.reset();
  // Drop the unread state + the persisted 1:1 read watermarks (the store
  // clears them; same shared-device privacy contract as the transcripts).
  unreadStore.reset();
  // Drop the status card's cached agent rows/contexts (same shared-device
  // contract): the next user must not see the previous session's agents.
  statusCardStore.detach();
  await archeionSession.logout();
  // Drop any held composer drafts AFTER the logout round-trip: the page
  // stays mounted (and typable) until the gate redirects, so an earlier
  // reset would let a keystroke during the await re-persist the draft.
  chatDraftsStore.reset();
}
