<script lang="ts">
  import { archeionSession } from "../lib/session/archeion.svelte";
  import { roomsStore } from "../lib/session/rooms.svelte";
  import { roomConversationsStore } from "../lib/session/roomConversations.svelte";
  import { navigate } from "../lib/shell/port.ts";
  import RoomsDashboard from "../components/rooms/RoomsDashboard.svelte";
  import {
    rooms_title,
    rooms_retrying,
    common_loading,
    nav_rooms,
    nav_tagmata,
  } from "../paraglide/messages.js";

  // The registry is fetched by RootLayout's user_id $effect (it must load
  // regardless of which page the user lands on, and re-load on re-login); this
  // view reads it reactively. The dashboard derives each section's phase from
  // the store's loaded/error flags.
  const roomsPhase = $derived(
    roomsStore.roomsError
      ? "error"
      : roomsStore.roomsLoaded
        ? "loaded"
        : "loading",
  );
  const invitesPhase = $derived(
    roomsStore.invitesError
      ? "error"
      : roomsStore.invitesLoaded
        ? "loaded"
        : "loading",
  );
</script>

<svelte:head><title>{rooms_title()}</title></svelte:head>
<div class="h-full flex flex-col">
  <div class="flex-1 min-h-0">
    {#if archeionSession.user}
      <RoomsDashboard
        rooms={roomsStore.rooms}
        {roomsPhase}
        invites={roomsStore.invites}
        {invitesPhase}
        busy={roomsStore.creating}
        onCreate={async (opts) => {
          const id = await roomConversationsStore.createRoom(opts);
          navigate(`/rooms/${id}`);
        }}
        publicRooms={roomsStore.publicRooms}
        publicRoomsError={roomsStore.publicRoomsError}
        onJoinPublic={(roomId) =>
          roomsStore
            .joinPublicRoom(roomId)
            .then(() => navigate(`/rooms/${roomId}`))}
        onAcceptInvite={(inv) => roomsStore.acceptInvite(inv)}
        onOpenSettings={(roomId) => navigate(`/rooms/${roomId}/settings`)}
        onOpen={(roomId) => navigate(`/rooms/${roomId}`)}
      />
    {:else if archeionSession.authError}
      <div class="p-4">
        <p class="text-error-500 dark:text-error-400 text-sm">
          {archeionSession.authError}
        </p>
        <p class="opacity-60 text-sm">{rooms_retrying()}</p>
      </div>
    {:else}
      <div class="p-4"><p class="opacity-60">{common_loading()}</p></div>
    {/if}
  </div>
</div>
