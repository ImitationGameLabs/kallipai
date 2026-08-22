// Account mode/identity actions shared by the two account surfaces: the
// sidebar footer dropdown (AccountMenu) and the /account hub page (the
// small-viewport replacement for the dropdown). Pure action logic — every
// UI derivation (connection viewmodel, user-branch rendering) stays in the
// components so this module holds no presentation concerns.
import { agoraSession } from "./agora.svelte";
import { channelsStore } from "./channels.svelte";
import { roomsStore } from "./rooms.svelte";
import { roomConversationsStore } from "./roomConversations.svelte";
import { chatDraftsStore } from "./drafts.ts";
import { configStore } from "../config/config.svelte";
import { connectDirect } from "./connect.ts";
import { navigate } from "../shell/port.ts";

// Online: end the agora session (destroys the cookie -- distinct from
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
  await agoraSession.logout();
  // Drop any held composer drafts AFTER the logout round-trip: the page
  // stays mounted (and typable) until the gate redirects, so an earlier
  // reset would let a keystroke during the await re-persist the draft.
  chatDraftsStore.reset();
}

// Offline -> online: detach the tagma and flip the active mode. The agora
// session cookie survives offline mode (we never logout() on a switch), so a
// whoami() re-resolves the signed-in user with no re-auth. The retained
// offline creds stay on disk for the switch back. Non-destructive, so no
// confirm. The gate owns post-switch routing.
export async function switchToOnline() {
  channelsStore.detachLocal();
  await configStore.setActiveMode("online");
  void agoraSession.whoami();
}

// Online -> offline: if offline creds are already saved, reconnect to the
// tagma directly (re-auth-free); otherwise send the user to /connect for
// first-time setup. Drop open channels: offline mode does not render online
// chats, so their SSE subscriber would keep running (against the still-valid
// cookie) and update transcripts nobody sees. The race guard re-checks
// activeMode before attachLocal: if the user flipped back to online while the
// connect was in flight, close the stray transport instead of attaching it
// (avoids a held tagma transport).
export async function switchToOffline() {
  // Mode switch: tear down transports but PRESERVE the IndexedDB cache so the
  // offline path rehydrates from the same rows the online path wrote.
  channelsStore.tearDownAll();
  const offline = configStore.value?.offline;
  if (!offline) {
    await navigate("/connect");
    return;
  }
  await configStore.setActiveMode("offline");
  let connection;
  try {
    connection = await connectDirect(offline);
  } catch (e) {
    channelsStore.localError = e;
    return;
  }
  if (configStore.value?.activeMode === "offline") {
    await channelsStore.attachLocal(
      connection.transport,
      connection.conversationId,
    );
  } else {
    // Mode flipped before attach landed: tear down the transport we opened.
    // close() is synchronous (it only aborts the SSE fetch).
    connection.transport.close();
  }
}
