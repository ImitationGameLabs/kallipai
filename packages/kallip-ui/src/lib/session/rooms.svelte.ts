// RoomsStore: reactive ($state) wrapper for the multi-member room registry
// (`/v1/rooms`). A peer singleton to `agoraSession` (rooms are a separate
// concern from identity + the owner's tagmata), refreshed from a `$effect` in
// RootLayout keyed on the signed-in user.
//
// Error discipline mirrors the tagma block of agora.svelte.ts: list-fetch
// failures land in per-section error fields (`roomsError`, `invitesError`) and
// never blank signed-in state; mutations THROW on error so the caller surfaces
// it inline (a single failed invite/add must not blank the whole dashboard).
//
// The store caches only the rooms list + the caller's pending-invites inbox.
// `RoomView` carries no roster, so add/remove-tagma mutate nothing the store
// holds -- they just await + throw, no refresh.

import {
  type RoomInviteView,
  type RoomView,
  type Visibility,
} from "@kallipai/kallip-lesche-client";
import { participantIdForTagma } from "@kallipai/kallip-common";
import { agoraSession, lescheClientOrFail } from "./agora.svelte.ts";

function messageOf(e: unknown): string {
  if (e instanceof Error) return e.message;
  return String(e);
}

class RoomsStore {
  // The caller's rooms (where they are a current member). Registry view only;
  // no roster (the list endpoint carries just room_id + created_at).
  rooms: RoomView[] = $state([]);
  roomsLoaded = $state(false);
  roomsError: string | null = $state(null);

  // The caller's pending invites (the invitee's inbox), newest first.
  invites: RoomInviteView[] = $state([]);
  invitesLoaded = $state(false);
  invitesError: string | null = $state(null);

  // Public (plaintext, open-access) rooms the caller may join, newest-created.
  // Refreshed alongside the membership lists; a join mutates membership so a
  // re-fetch of `rooms` follows it.
  publicRooms: RoomView[] = $state([]);
  publicRoomsError: string | null = $state(null);

  // True while a create is in flight (disables the New Room buttons).
  creating = $state(false);

  /** Fetch both lists in parallel. Each writes its own error field so one
   * failure does not blank the other; stale lists + loaded flags are left in
   * place on error. Resolves when both settles. */
  async refresh(): Promise<void> {
    const client = lescheClientOrFail();
    const [roomsRes, invitesRes, publicRes] = await Promise.allSettled([
      client.listRooms(),
      client.listMyRoomInvites(),
      client.listPublicRooms(),
    ]);
    if (roomsRes.status === "fulfilled") {
      this.rooms = roomsRes.value;
      this.roomsLoaded = true;
      this.roomsError = null;
    } else {
      this.roomsError = messageOf(roomsRes.reason);
    }
    if (invitesRes.status === "fulfilled") {
      this.invites = invitesRes.value;
      this.invitesLoaded = true;
      this.invitesError = null;
    } else {
      this.invitesError = messageOf(invitesRes.reason);
    }
    if (publicRes.status === "fulfilled") {
      this.publicRooms = publicRes.value;
      this.publicRoomsError = null;
    } else {
      this.publicRoomsError = messageOf(publicRes.reason);
    }
  }

  /** Create a room; the caller is the founding member. Prepend the response so
   * the new room lands on top. Sets `creating` for the busy affordance and
   * THROWS on error (the create dialog surfaces the failure via its `error`
   * prop). `roomsError` is reserved for the list fetch -- a failed create must
   * not blank the dashboard section. Returns the created room so a caller can
   * navigate to it. */
  async createRoom(opts: {
    name: string;
    description?: string;
    visibility?: Visibility;
  }): Promise<RoomView> {
    this.creating = true;
    try {
      const created = await lescheClientOrFail().createRoom(opts);
      this.rooms = [created, ...this.rooms];
      this.roomsLoaded = true;
      return created;
    } finally {
      this.creating = false;
    }
  }

  /** Join a public room (open-access). On success the room enters `rooms`
   * (membership), so a full `refresh()` reconciles both lists. THROWS on error
   * (a 403 means the room is private -- use the invite flow). */
  async joinPublicRoom(roomId: string): Promise<void> {
    await lescheClientOrFail().joinRoom(roomId);
    await this.refresh();
  }

  /** Accept a pending invite. The room joins `rooms` and the invite leaves the
   * inbox -- a splice would be fragile, so re-fetch both. THROWS on error. */
  async acceptInvite(invite: RoomInviteView): Promise<void> {
    await lescheClientOrFail().acceptRoomInvite(
      invite.room_id,
      invite.invite_id,
    );
    await this.refresh();
  }

  /** Leave a room (self-removal). Re-fetch to reconcile. THROWS on error. */
  async leaveRoom(roomId: string): Promise<void> {
    const memberId = agoraSession.participantId;
    if (!memberId) throw new Error("no signed-in user");
    await lescheClientOrFail().removeRoomMember(roomId, memberId);
    await this.refresh();
  }

  /** Remove another member from a room (creator admin). Keyed by the target's
   * member id; the server enforces that only the creator may remove another.
   * Changes nothing the store caches (no roster); the page re-fetches. THROWS
   * on error. */
  async removeMember(roomId: string, memberId: string): Promise<void> {
    await lescheClientOrFail().removeRoomMember(roomId, memberId);
  }

  /** Pull a tagma you own out of a room (owner admin, works cross-room). The
   * tagma's member id is derived from its tagma id. THROWS on error. */
  async removeTagmaFromRoom(roomId: string, tagmaId: string): Promise<void> {
    const memberId = await participantIdForTagma(tagmaId);
    await lescheClientOrFail().removeRoomMember(roomId, memberId);
  }

  /** Invite a user to a room by @username. Changes nothing the store caches
   * (invites-sent are not listed); just await + throw. THROWS on error. */
  async inviteUser(roomId: string, inviteeUsername: string): Promise<void> {
    await lescheClientOrFail().createRoomInvite(roomId, inviteeUsername);
  }

  /** Add a tagma to a room. Changes nothing the store caches (no roster); just
   * await + throw. THROWS on error. */
  async addTagma(roomId: string, tagmaId: string): Promise<void> {
    await lescheClientOrFail().addRoomTagma(roomId, tagmaId);
  }

  /** Drop all registry state (logout). Cleared so the next user's login never
   * sees the prior user's rooms/invites -- the lists are per-user plaintext and
   * must not linger across sessions on a shared device. */
  reset(): void {
    this.rooms = [];
    this.roomsLoaded = false;
    this.roomsError = null;
    this.invites = [];
    this.invitesLoaded = false;
    this.invitesError = null;
    this.publicRooms = [];
    this.publicRoomsError = null;
  }
}

export const roomsStore = new RoomsStore();
