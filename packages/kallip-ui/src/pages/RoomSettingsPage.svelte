<script lang="ts">
  // The room management surface as a PAGE (not a modal): room info, the live
  // member roster, the membership actions (invite a user, add a tagma -- with a
  // picker over the caller's own enrolled tagmata plus a manual id fallback),
  // and a confirmed Leave in a danger zone. Reached from the conversation
  // header's gear and the room card kebab (`/rooms/{id}/settings`).
  //
  // The page owns only its per-action busy/error state + the roster fetch; every
  // mutation delegates to `roomsStore`. The roster is fetched directly (not via
  // roomConversationsStore.refreshRoster, which is gated on the transcript being
  // open) so the page works standalone, and re-fetched after each mutation.
  import { ArrowLeft, ChevronDown } from "@lucide/svelte";
  import { Menu, Portal } from "@skeletonlabs/skeleton-svelte";
  import { participantIdForTagma } from "@kallipai/kallip-common";
  import type {
    RoomMemberProfile,
    RoomRosterView,
  } from "@kallipai/kallip-lesche-client";
  import ConfirmDialog from "../components/ConfirmDialog.svelte";
  import {
    agoraSession,
    lescheClientOrFail,
  } from "../lib/session/agora.svelte";
  import { roomsStore } from "../lib/session/rooms.svelte";
  import { roomConversationsStore } from "../lib/session/roomConversations.svelte";
  import MemberRow from "../components/rooms/MemberRow.svelte";
  import { navigate } from "../lib/shell/port.ts";
  import { getLocale } from "../paraglide/runtime.js";

  import {
    common_loading,
    common_add,
    common_remove,
    room_label_fallback,
    room_id_label,
    room_public_badge,
    room_private_badge,
    room_member_one,
    room_member_other,
    room_members,
    roomsettings_title,
    roomsettings_subtitle,
    roomsettings_back_aria,
    roomsettings_created,
    roomsettings_members_error,
    roomsettings_invite_title,
    roomsettings_invite,
    roomsettings_invite_hint,
    roomsettings_invite_sent,
    roomsettings_add_tagma,
    roomsettings_add_tagma_hint,
    roomsettings_pick_tagma,
    roomsettings_tagma_id_placeholder,
    roomsettings_no_tagmata,
    roomsettings_loading_tagmata,
    tagma_fallback_label,
    roomsettings_added,
    roomsettings_tagma_added,
    roomsettings_danger,
    roomsettings_leave,
    roomsettings_leave_title,
    roomsettings_leave_desc_named,
    roomsettings_leave_desc,
    roomsettings_leaving,
    roomsettings_leave_confirm,
    roomsettings_leave_failed,
    roomsettings_remove_title,
    roomsettings_remove_desc,
    roomsettings_removing,
    roomsettings_remove_failed,
  } from "../paraglide/messages.js";
  let { roomId }: { roomId: string } = $props();

  const room = $derived(
    roomsStore.rooms.find((r) => r.room_id === roomId) ?? null,
  );
  const roomLabel = $derived(
    room?.name || room_label_fallback({ id: roomId.slice(0, 8) }),
  );
  const isPublic = $derived(room?.visibility === "public");

  // The live roster. Independent of the conversation store (no transcript
  // requirement), refreshed after each mutation below.
  let roster = $state<RoomRosterView | null>(null);
  let rosterError = $state<string | null>(null);

  // Invite state.
  let inviteeUsername = $state("");
  let inviteBusy = $state(false);
  let inviteError = $state<string | null>(null);
  let inviteDone = $state(false);

  // Add-tagma state (manual id input).
  let manualTagmaId = $state("");
  let addBusy = $state(false);
  let addError = $state<string | null>(null);
  let addDone = $state(false);

  // Leave state.
  let leaveOpen = $state(false);
  let leaveBusy = $state(false);
  let leaveError = $state<string | null>(null);

  // Remove-member state (creator admin over another member).
  let removeTarget = $state<RoomMemberProfile | null>(null);
  let removeBusy = $state(false);
  let removeError = $state<string | null>(null);

  // The caller's own enrolled tagmata: the picker source for the add-tagma
  // shortcut (pending tagmata are not enrolled and cannot join a room).
  const myTagmas = $derived(
    agoraSession.tagmata.filter((t) => t.state === "enrolled"),
  );

  // Map tagma_id -> participant id for my enrolled tagmata, so the picker can
  // mark already-added tagmas: the roster carries derived participant ids, not
  // raw tagma ids. participantIdForTagma is async, so derive the map in an
  // effect (stale-guarded on the cleanup).
  let myTagmaPids = $state<Record<string, string>>({});
  $effect(() => {
    const ids = myTagmas.map((t) => t.tagma_id);
    const next: Record<string, string> = {};
    let stale = false;
    for (const id of ids) {
      void participantIdForTagma(id).then((pid) => {
        if (stale) return;
        next[id] = pid;
        myTagmaPids = { ...next };
      });
    }
    return () => {
      stale = true;
    };
  });

  // The roster's participant ids, for the add-tagma picker's "already added"
  // mark (the roster carries derived participant ids, not raw tagma ids).
  const memberPids = $derived(
    new Set((roster?.members ?? []).map((m) => m.id)),
  );

  function isMyTagmaAdded(tagmaId: string): boolean {
    const pid = myTagmaPids[tagmaId];
    return !!pid && memberPids.has(pid);
  }

  // The single source of truth for "how a roster fetch becomes state writes".
  // `isStale` gates the writes so a late response from a PREVIOUS room cannot
  // overwrite the current view on a fast client-side nav.
  async function loadRoster(id: string, isStale: () => boolean): Promise<void> {
    try {
      const next = await lescheClientOrFail().fetchRoomRoster(id);
      if (isStale()) return;
      roster = next;
      rosterError = null;
    } catch (e) {
      if (isStale()) return;
      rosterError = e instanceof Error ? e.message : String(e);
    }
  }

  // Re-fetch on mount AND whenever the room changes: SvelteKit reuses this page
  // across `[id]` param changes (no remount), so onMount alone would leave a
  // stale roster on a client-side nav. Stale-guarded (mirrors the myTagmaPids
  // effect above): `roomId` is read here, so it is the sole dependency, and
  // resetting `roster` shows the existing "Loading..." state for the new room.
  $effect(() => {
    const id = roomId;
    roster = null;
    rosterError = null;
    let stale = false;
    void loadRoster(id, () => stale);
    return () => {
      stale = true;
    };
  });

  // Manual post-mutation refresh (addTagma, confirmRemove): always the current
  // room, so it never goes stale.
  async function refreshRoster(): Promise<void> {
    await loadRoster(roomId, () => false);
  }

  async function invite(e: Event): Promise<void> {
    e.preventDefault();
    const handle = inviteeUsername.trim();
    if (!handle || inviteBusy) return;
    inviteBusy = true;
    inviteError = null;
    inviteDone = false;
    try {
      await roomsStore.inviteUser(roomId, handle);
      inviteeUsername = "";
      inviteDone = true;
    } catch (err) {
      inviteError = err instanceof Error ? err.message : String(err);
    } finally {
      inviteBusy = false;
    }
  }

  async function addTagma(targetId: string): Promise<void> {
    const id = targetId.trim();
    if (!id || addBusy) return;
    addBusy = true;
    addError = null;
    addDone = false;
    try {
      await roomsStore.addTagma(roomId, id);
      await refreshRoster();
      addDone = true;
      manualTagmaId = "";
    } catch (err) {
      addError = err instanceof Error ? err.message : String(err);
    } finally {
      addBusy = false;
    }
  }

  async function confirmLeave(): Promise<void> {
    if (leaveBusy) return;
    leaveBusy = true;
    leaveError = null;
    try {
      await roomsStore.leaveRoom(roomId);
      roomConversationsStore.dispose(roomId);
      navigate("/rooms");
    } catch (err) {
      leaveError = err instanceof Error ? err.message : String(err);
      leaveBusy = false;
    }
  }

  function openRemove(m: RoomMemberProfile): void {
    removeTarget = m;
    removeError = null;
  }

  async function confirmRemove(): Promise<void> {
    const target = removeTarget;
    if (!target || removeBusy) return;
    removeBusy = true;
    removeError = null;
    try {
      await roomsStore.removeMember(roomId, target.id);
      await refreshRoster();
      removeTarget = null;
    } catch (err) {
      removeError = err instanceof Error ? err.message : String(err);
    } finally {
      removeBusy = false;
    }
  }
</script>

<svelte:head><title>{roomsettings_title()}</title></svelte:head>

<div class="flex flex-col h-full">
  <header
    class="px-4 py-2 border-b border-surface-200-800 flex items-center gap-2"
  >
    <button
      type="button"
      class="size-8 grid place-items-center rounded-base preset-tonal-surface hover:preset-filled-surface-500 shrink-0"
      aria-label={roomsettings_back_aria()}
      onclick={() => navigate(`/rooms/${roomId}`)}
    >
      <ArrowLeft class="size-4" />
    </button>
    <div class="flex flex-col min-w-0 flex-1">
      <p class="text-sm font-semibold truncate">{roomLabel}</p>
      <p class="text-xs opacity-50 truncate">{roomsettings_subtitle()}</p>
    </div>
    {#if isPublic}
      <span class="text-xs preset-tonal-surface px-2 py-0.5 rounded-base"
        >{room_public_badge()}</span
      >
    {/if}
  </header>

  <div class="flex-1 min-h-0 overflow-auto">
    <div class="mx-auto w-full max-w-2xl p-4 flex flex-col gap-4 min-h-full">
      <!-- Room info -->
      <section
        class="card preset-tonal-surface p-4 flex flex-col gap-1 text-sm"
      >
        {#if room?.description}
          <p class="opacity-80">{room.description}</p>
        {/if}
        <div class="flex flex-wrap items-center gap-2 text-xs opacity-60">
          {#if isPublic}
            <span class="preset-tonal-surface px-2 py-0.5 rounded-base"
              >{room_public_badge()}</span
            >
          {:else}
            <span class="preset-tonal-surface px-2 py-0.5 rounded-base"
              >{room_private_badge()}</span
            >
          {/if}
          {#if roster}
            <span>
              {roster.members.length === 1
                ? room_member_one({ count: roster.members.length })
                : room_member_other({ count: roster.members.length })}
            </span>
          {/if}
          {#if room}
            <span
              >{roomsettings_created({
                date: new Date(room.created_at).toLocaleString(getLocale()),
              })}</span
            >
          {/if}
        </div>
        <p class="text-xs opacity-50 break-all">
          <span class="opacity-70">{room_id_label()}</span><span
            class="font-mono">{roomId}</span
          >
        </p>
      </section>

      <!-- Members -->
      <section class="flex flex-col gap-2">
        <h2 class="text-sm font-semibold opacity-70">{room_members()}</h2>
        {#if rosterError}
          <p class="text-xs text-error-500 dark:text-error-400">
            {roomsettings_members_error({ error: rosterError })}
          </p>
        {:else if !roster}
          <p class="text-sm opacity-60">{common_loading()}</p>
        {:else}
          <div class="flex flex-col gap-1">
            {#each roster.members as m (m.id)}
              <MemberRow
                member={m}
                selfId={agoraSession.participantId}
                isCreator={roster.is_creator &&
                  m.id === agoraSession.participantId}
                removable={roster.is_creator &&
                  m.id !== agoraSession.participantId}
                onRemove={() => openRemove(m)}
              />
            {/each}
          </div>
        {/if}
      </section>

      <!-- Invite a user -->
      {#if !isPublic}
        <section class="flex flex-col gap-2">
          <h2 class="text-sm font-semibold opacity-70">
            {roomsettings_invite_title()}
          </h2>
          <form class="flex flex-col gap-1" onsubmit={invite}>
            <div class="flex flex-wrap gap-2">
              <input
                class="input flex-1 text-sm"
                placeholder="@username"
                bind:value={inviteeUsername}
                disabled={inviteBusy}
              />
              <button
                type="submit"
                class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500 disabled:opacity-60"
                disabled={inviteBusy || !inviteeUsername.trim()}
              >
                {inviteBusy ? "…" : roomsettings_invite()}
              </button>
            </div>
            <p class="text-xs opacity-50">
              {roomsettings_invite_hint()}
            </p>
            {#if inviteError}
              <p class="text-xs text-error-500 dark:text-error-400">
                {inviteError}
              </p>
            {:else if inviteDone}
              <p class="text-xs text-success-500 dark:text-success-400">
                {roomsettings_invite_sent()}
              </p>
            {/if}
          </form>
        </section>
      {/if}

      <!-- Add a tagma -->
      <section class="flex flex-col gap-2">
        <h2 class="text-sm font-semibold opacity-70">
          {roomsettings_add_tagma()}
        </h2>
        <p class="text-xs opacity-60">
          {roomsettings_add_tagma_hint()}
        </p>

        <Menu
          positioning={{ placement: "bottom-start", gutter: 8 }}
          onSelect={(e) => (manualTagmaId = e.value)}
        >
          <Menu.Trigger
            class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500 self-start flex items-center gap-2 disabled:opacity-60"
            disabled={addBusy || !agoraSession.tagmataLoaded}
          >
            {roomsettings_pick_tagma()}
            <ChevronDown class="size-4" />
          </Menu.Trigger>
          <Portal>
            <Menu.Positioner>
              <Menu.Content
                class="card preset-tonal-surface p-1 min-w-[16rem] max-h-[50vh] overflow-auto"
              >
                {#if myTagmas.length === 0}
                  <p class="px-3 py-2 text-sm opacity-60">
                    {agoraSession.tagmataLoaded
                      ? roomsettings_no_tagmata()
                      : roomsettings_loading_tagmata()}
                  </p>
                {:else}
                  {#each myTagmas as t (t.tagma_id)}
                    {@const added = isMyTagmaAdded(t.tagma_id)}
                    <Menu.Item
                      value={t.tagma_id}
                      disabled={added}
                      class="flex items-center justify-between gap-2 px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500 disabled:opacity-60"
                    >
                      <span class="truncate">
                        {t.label ??
                          tagma_fallback_label({
                            id: t.tagma_id.slice(0, 8),
                          })}
                      </span>
                      {#if added}
                        <span class="text-xs opacity-60 shrink-0"
                          >{roomsettings_added()}</span
                        >
                      {/if}
                    </Menu.Item>
                  {/each}
                {/if}
              </Menu.Content>
            </Menu.Positioner>
          </Portal>
        </Menu>

        <form
          class="flex flex-wrap gap-2"
          onsubmit={(e) => {
            e.preventDefault();
            void addTagma(manualTagmaId);
          }}
        >
          <input
            class="input flex-1 text-sm"
            placeholder={roomsettings_tagma_id_placeholder()}
            bind:value={manualTagmaId}
            disabled={addBusy}
          />
          <button
            type="submit"
            class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500 disabled:opacity-60"
            disabled={addBusy || !manualTagmaId.trim()}
          >
            {addBusy ? "…" : common_add()}
          </button>
        </form>
        {#if addError}
          <p class="text-xs text-error-500 dark:text-error-400">{addError}</p>
        {:else if addDone}
          <p class="text-xs text-success-500 dark:text-success-400">
            {roomsettings_tagma_added()}
          </p>
        {/if}
      </section>

      <!-- Danger zone -->
      <section class="flex flex-col gap-2 mt-2">
        <h2 class="text-sm font-semibold text-error-500 dark:text-error-400">
          {roomsettings_danger()}
        </h2>
        <button
          type="button"
          class="btn btn-sm preset-outlined-surface-500 self-start hover:preset-filled-error-500 disabled:opacity-60"
          onclick={() => {
            leaveError = null;
            leaveOpen = true;
          }}
        >
          {roomsettings_leave()}
        </button>
      </section>
    </div>
  </div>
</div>

<ConfirmDialog
  open={leaveOpen}
  title={roomsettings_leave_title()}
  description={room?.name
    ? roomsettings_leave_desc_named({ name: room.name })
    : roomsettings_leave_desc()}
  confirmLabel={leaveBusy
    ? roomsettings_leaving()
    : roomsettings_leave_confirm()}
  busy={leaveBusy}
  tone="danger"
  error={leaveError ? roomsettings_leave_failed({ error: leaveError }) : null}
  onConfirm={confirmLeave}
  onCancel={() => {
    leaveOpen = false;
    leaveError = null;
  }}
/>

<ConfirmDialog
  open={removeTarget !== null}
  title={roomsettings_remove_title()}
  description={removeTarget
    ? roomsettings_remove_desc({ handle: removeTarget.handle })
    : ""}
  confirmLabel={removeBusy ? roomsettings_removing() : common_remove()}
  busy={removeBusy}
  tone="danger"
  error={removeError
    ? roomsettings_remove_failed({ error: removeError })
    : null}
  onConfirm={confirmRemove}
  onCancel={() => {
    removeTarget = null;
    removeError = null;
  }}
/>
