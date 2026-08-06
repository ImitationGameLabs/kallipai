<script lang="ts">
  // The rooms dashboard: a centered column listing the caller's rooms + their
  // pending-invites inbox, with a single "New Room" primary action that opens a
  // create form (CreateRoomDialog). One rooms section + one invites section,
  // each with its own load phase (the two lists come from independent endpoints
  // and fail independently). Prop-driven; the owning store does all fetching +
  // mutations. Per-room management (invite/add-tagma/leave) lives on the room's
  // settings PAGE, opened from each card's kebab via onOpenSettings.
  import type {
    RoomInviteView,
    RoomView,
  } from "@kallipai/kallip-lesche-client";
  import type { SectionPhase } from "../../lib/phase.ts";
  import RoomCard from "./RoomCard.svelte";
  import CreateRoomDialog, {
    type CreateRoomOpts,
  } from "./CreateRoomDialog.svelte";

  let {
    rooms,
    roomsPhase,
    invites,
    invitesPhase,
    publicRooms = [],
    publicRoomsError = null,
    busy = false,
    onCreate,
    onAcceptInvite,
    onOpenSettings,
    onOpen,
    onJoinPublic,
  }: {
    rooms: RoomView[];
    roomsPhase: SectionPhase;
    invites: RoomInviteView[];
    invitesPhase: SectionPhase;
    // Public (plaintext, open-access) rooms the caller may join.
    publicRooms?: RoomView[];
    // Set when the public-discovery fetch failed; surfaced on the section.
    publicRoomsError?: string | null;
    // True while a create is in flight (disables the create affordance).
    busy?: boolean;
    onCreate?: (opts: CreateRoomOpts) => Promise<unknown> | unknown;
    onAcceptInvite?: (invite: RoomInviteView) => void;
    // Navigate to a room's settings page (the management surface).
    onOpenSettings?: (roomId: string) => void;
    onOpen?: (roomId: string) => void;
    onJoinPublic?: (roomId: string) => Promise<unknown> | unknown;
  } = $props();

  // First-run empty state: nothing to show but the primary action. Requires
  // `loaded` so the hero does not flash during the initial fetch.
  const isEmpty = $derived(
    roomsPhase === "loaded" && rooms.length === 0 && invites.length === 0,
  );

  // Create dialog: owns its open + error state here (the store call happens in
  // the owning page's `onCreate` wrapper, which throws on failure). Closes on
  // success, surfaces the error on failure.
  let createOpen = $state(false);
  let createError = $state<string | null>(null);

  async function doCreate(opts: CreateRoomOpts): Promise<void> {
    if (!onCreate) return;
    createError = null;
    try {
      await onCreate(opts);
      createOpen = false;
    } catch (err) {
      createError = err instanceof Error ? err.message : String(err);
    }
  }

  // Per-invite accept state: which invite is accepting (one at a time) + the
  // per-invite error. Catches the double-accept 409 so it is not an unhandled
  // rejection, and shows the failure inline on its own row.
  let acceptingId: string | null = $state(null);
  let acceptErrors: Record<string, string> = $state({});

  async function accept(inv: RoomInviteView): Promise<void> {
    if (acceptingId || !onAcceptInvite) return;
    acceptingId = inv.invite_id;
    const { [inv.invite_id]: _omit, ...rest } = acceptErrors;
    acceptErrors = rest;
    try {
      await onAcceptInvite(inv);
    } catch (err) {
      acceptErrors = {
        ...acceptErrors,
        [inv.invite_id]: err instanceof Error ? err.message : String(err),
      };
    } finally {
      acceptingId = null;
    }
  }

  // Per-public-room join state: which room is joining + per-room error.
  let joiningId: string | null = $state(null);
  let joinErrors: Record<string, string> = $state({});

  // Public rooms the caller has NOT already joined (joined ones show in the
  // rooms section above, so hide them from the discovery list).
  const joinablePublic = $derived(
    publicRooms.filter((r) => !rooms.some((m) => m.room_id === r.room_id)),
  );

  async function joinPublic(roomId: string): Promise<void> {
    if (joiningId || !onJoinPublic) return;
    joiningId = roomId;
    const { [roomId]: _omit, ...rest } = joinErrors;
    joinErrors = rest;
    try {
      await onJoinPublic(roomId);
    } catch (err) {
      joinErrors = {
        ...joinErrors,
        [roomId]: err instanceof Error ? err.message : String(err),
      };
    } finally {
      joiningId = null;
    }
  }
</script>

<div class="h-full overflow-auto">
  <div class="mx-auto w-full max-w-2xl p-4 flex flex-col gap-4 min-h-full">
    {#if isEmpty}
      <div class="flex flex-1 flex-col items-center justify-center gap-3">
        <button
          type="button"
          class="card preset-filled-primary-500 text-on-primary-500 w-full max-w-md p-8 text-center transition hover:brightness-110 disabled:opacity-60"
          disabled={busy}
          onclick={() => {
            createError = null;
            createOpen = true;
          }}
        >
          <div class="text-2xl font-semibold">
            {busy ? "Creating…" : "New Room"}
          </div>
          <div class="opacity-80">
            Invite-only and private by default, or open a public room.
          </div>
        </button>
      </div>
    {:else}
      <!-- Pending invites first (time-sensitive), then the rooms list. -->
      {#if invites.length > 0 || invitesPhase === "loading" || invitesPhase === "error"}
        <section class="flex flex-col gap-2">
          <h2 class="text-sm font-semibold opacity-70">Pending invites</h2>
          {#if invitesPhase === "loading"}
            <p class="text-sm opacity-60">Loading...</p>
          {:else if invitesPhase === "error"}
            <p class="text-sm text-error-500">Failed to load invites.</p>
          {:else}
            {#each invites as inv (inv.invite_id)}
              <!-- The invite carries only a room id (no name); the room is not in
                   the caller's registry yet, so show the id prefix. -->
              <div class="card flex flex-col gap-2 p-3 text-sm">
                <div class="flex items-center justify-between gap-2">
                  <div class="flex flex-col">
                    <span class="font-mono">{inv.room_id.slice(0, 8)}</span>
                    <span class="text-xs opacity-50">
                      from {inv.invited_by} · expires
                      {new Date(inv.expires_at).toLocaleDateString()}</span
                    >
                  </div>
                  {#if onAcceptInvite}
                    <button
                      type="button"
                      class="btn btn-sm preset-filled-primary-500 disabled:opacity-60"
                      disabled={acceptingId === inv.invite_id}
                      onclick={() => accept(inv)}
                    >
                      {acceptingId === inv.invite_id ? "…" : "Accept"}
                    </button>
                  {/if}
                </div>
                {#if acceptErrors[inv.invite_id]}
                  <p class="text-xs text-error-500">
                    {acceptErrors[inv.invite_id]}
                  </p>
                {/if}
              </div>
            {/each}
          {/if}
        </section>
      {/if}

      {#if onJoinPublic && (joinablePublic.length > 0 || publicRoomsError)}
        <section class="flex flex-col gap-2">
          <h2 class="text-sm font-semibold opacity-70">Public rooms</h2>
          {#if publicRoomsError}
            <p class="text-xs text-error-500">
              Could not load public rooms: {publicRoomsError}
            </p>
          {:else}
            {#each joinablePublic as room (room.room_id)}
              <div
                class="card flex items-center justify-between gap-3 p-3 text-sm"
              >
                <div class="flex flex-col min-w-0">
                  <span class="font-medium truncate">
                    {room.name || `room ${room.room_id.slice(0, 8)}`}
                  </span>
                  {#if room.description}
                    <span class="text-xs opacity-60 truncate"
                      >{room.description}</span
                    >
                  {/if}
                </div>
                <button
                  type="button"
                  class="btn btn-sm preset-filled-primary-500 disabled:opacity-60 shrink-0"
                  disabled={joiningId === room.room_id}
                  onclick={() => joinPublic(room.room_id)}
                >
                  {joiningId === room.room_id ? "…" : "Join"}
                </button>
              </div>
              {#if joinErrors[room.room_id]}
                <p class="text-xs text-error-500 -mt-1">
                  {joinErrors[room.room_id]}
                </p>
              {/if}
            {/each}
          {/if}
        </section>
      {/if}

      <section class="flex flex-col gap-3">
        {#if roomsPhase === "loading"}
          <p class="text-sm opacity-60">Loading...</p>
        {:else if roomsPhase === "error"}
          <p class="text-sm text-error-500">Failed to load rooms.</p>
        {:else}
          {#each rooms as room (room.room_id)}
            <RoomCard
              {room}
              onOpen={onOpen ? () => onOpen(room.room_id) : undefined}
              onSettings={onOpenSettings
                ? () => onOpenSettings(room.room_id)
                : undefined}
            />
          {/each}
        {/if}
        {#if onCreate}
          <button
            type="button"
            class="card preset-tonal-surface flex items-center justify-center gap-2 py-4 text-sm font-medium transition hover:preset-filled-primary-500 disabled:opacity-60"
            disabled={busy}
            onclick={() => {
              createError = null;
              createOpen = true;
            }}
          >
            {busy ? "Creating…" : "+ New Room"}
          </button>
        {/if}
      </section>
    {/if}
  </div>
</div>

<CreateRoomDialog
  open={createOpen}
  {busy}
  error={createError}
  onCreate={doCreate}
  onCancel={() => {
    createOpen = false;
    createError = null;
  }}
/>
