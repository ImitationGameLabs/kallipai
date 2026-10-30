<script lang="ts">
  // Catalog set edit dialog: the description field, the member order (the
  // failover sequence) as a draggable card list, and each member's fields
  // as an inline form the card opens. This is the face's one composite
  // edit, so the dialog holds a local draft (dialog-scoped, not a page
  // draft session) and hands the whole of it back: the page lands changed
  // rows as PUTs, then the description, then the order. Sets are never
  // here: the name is the registry key.
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import {
    common_cancel,
    common_save,
    manage_gateway_set_members_order_hint,
    manage_profiles_set_dialog_edit_title,
    manage_profiles_set_dialog_description_label,
  } from "../../../paraglide/messages.js";
  import GatewayMemberCard from "../../gateway/GatewayMemberCard.svelte";
  import type {
    AdminProfileRow,
    AdminSetRow,
  } from "../../../lib/manage/gateway/client.ts";
  import {
    membershipWrites,
    firstOffending,
    memberEditOf,
    memberIds,
    moveMember,
    reseed,
    type MemberEdit,
    type SetSaveDraft,
  } from "../../../lib/manage/gateway/setDraft.ts";

  let {
    open,
    set,
    busy = false,
    error = null,
    landed = [],
    onSave,
    onCancel,
  }: {
    open: boolean;
    /** The set under edit (null = nothing open; the dialog idles). */
    set: AdminSetRow | null;
    /** A save is in flight: the footer and the cards are locked. */
    busy?: boolean;
    /** The server's reason for the last failed save (the dialog stays). */
    error?: string | null;
    /** The rows whose PUT landed before a save stopped: a re-read reseeds
     * these only, so a row that never reached the registry keeps its draft. */
    landed?: readonly string[];
    onSave: (result: SetSaveDraft) => void;
    onCancel: () => void;
  } = $props();

  // The local draft: the description, the member rows (rows, not bare
  // ids, so the cards keep rendering during the reorder), the per-row
  // field edits, and which row's form is open. The draft seeds from the
  // set the page hands; a re-read after a failed save goes through
  // setDraft's reseed: landed rows take the registry row, a row that never
  // reached the registry keeps its draft, and an untouched order returns.
  let descriptionDraft = $state("");
  let members = $state<AdminProfileRow[]>([]);
  let edits = $state<Record<string, MemberEdit>>({});
  let editingId = $state<string | null>(null);
  let dragFrom = $state<number | null>(null);
  let dragOver = $state<number | null>(null);
  let seeded: AdminSetRow | null = null;
  let seededKey: string | null = null;
  // The member ids the draft was seeded with: reseed reads them to tell the
  // user's sequence from the seed's, and to keep a row the registry cannot
  // hold yet. Each re-seed hands back the ids it just read, so the next one
  // compares against the registry's order rather than a stale seed.
  let seededIds = $state<readonly string[]>([]);
  $effect(() => {
    if (!open) {
      // A closed dialog seeds from scratch when it opens again, even on
      // the same set: what it held was a draft of the last visit.
      seeded = null;
      seededKey = null;
      return;
    }
    if (set === null || set === seeded) return;
    seeded = set;
    const key = `${set.collection_name}/${set.name}`;
    const fresh = key !== seededKey;
    seededKey = key;
    if (fresh) {
      descriptionDraft = set.description;
      members = [...set.profiles];
      edits = {};
      editingId = null;
      seededIds = memberIds(set.profiles);
      dragFrom = null;
      dragOver = null;
      return;
    }
    // The same set, re-read after a failed save: setDraft's reseed decides
    // what stays a draft (landed rows take the registry row, an untouched
    // order comes back whole), so a save never carries a sequence someone
    // else moved.
    const next = reseed({
      registry: set.profiles,
      members,
      edits,
      editingId,
      landed,
      seededIds,
    });
    members = next.members;
    edits = next.edits;
    editingId = next.editingId;
    seededIds = next.seededIds;
  });

  // Only a save in flight greys the button. The description is its own
  // write and the order write carries the whole membership, so a set
  // may be saved with no members at all. A malformed draft does not
  // grey it either: a grey button over a closed form explains nothing,
  // so save opens the offending row instead.
  const canSubmit = $derived(!busy);

  function clearDrag(): void {
    dragFrom = null;
    dragOver = null;
  }

  function reorder(fromIdx: number, toIdx: number): void {
    // A save in flight freezes the order: the order write already carries
    // the sequence the draft had when it left.
    if (busy) return;
    if (fromIdx === toIdx) return;
    const next = moveMember(members, fromIdx, toIdx - fromIdx);
    if (next === members) return;
    members = [...next];
  }

  function step(idx: number, delta: number): void {
    if (busy) return;
    const next = moveMember(members, idx, delta);
    if (next === members) return;
    members = [...next];
  }

  // Removal joins the draft: the row leaves the list here, and the
  // membership change lands on save. Its field edits leave with it: the
  // member is leaving the set, so those values have no row left to land on.
  function removeAt(idx: number): void {
    // The card's remove button is disabled while a save runs; the same rule
    // holds here, so a stale click cannot edit a draft the save snapshotted.
    if (busy) return;
    const next = [...members];
    const [removed] = next.splice(idx, 1);
    if (removed !== undefined) {
      if (editingId === removed.profile_id) editingId = null;
      const rest = { ...edits };
      delete rest[removed.profile_id];
      edits = rest;
    }
    members = next;
  }

  // The form draft is seeded from the row the first time it opens, so a
  // toggle back and forth keeps what was typed.
  function toggleEdit(member: AdminProfileRow): void {
    const id = member.profile_id;
    if (editingId === id) {
      editingId = null;
      return;
    }
    editingId = id;
    if (edits[id] === undefined) {
      edits = { ...edits, [id]: memberEditOf(member) };
    }
  }

  function applyEdit(id: string, next: MemberEdit): void {
    edits = { ...edits, [id]: next };
  }

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open && !busy) onCancel();
  }

  // A drafted field the catalog would refuse stops the save instead of
  // riding along or being dropped with its form: the first offending row
  // opens, so the problem shows under the field that holds it.
  function submit(): void {
    if (!canSubmit || set === null) return;
    const offending = firstOffending(members, edits);
    if (offending !== undefined) {
      editingId = offending.profile_id;
      return;
    }
    onSave({
      name: set.name,
      description: descriptionDraft,
      members: memberIds(members),
      ...membershipWrites(set.profiles, members, edits, set.name),
    });
  }
</script>

<Dialog {open} {onOpenChange}>
  <Portal>
    <Dialog.Backdrop class="fixed inset-0 bg-surface-50-950/60 z-50" />
    <Dialog.Positioner class="fixed inset-0 z-50 grid place-items-center p-4">
      <Dialog.Content
        class="card preset-tonal-surface w-full max-w-xl p-6 flex flex-col gap-4 max-h-[85vh] overflow-y-auto"
      >
        <Dialog.Title class="text-lg font-semibold">
          {manage_profiles_set_dialog_edit_title()}
          <span class="font-mono opacity-80">{set?.name}</span>
        </Dialog.Title>

        <form
          class="flex flex-col gap-4"
          onsubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          <label class="flex flex-col gap-1">
            <span class="text-xs font-medium">
              {manage_profiles_set_dialog_description_label()}
            </span>
            <input class="input text-sm" bind:value={descriptionDraft} />
          </label>

          <p class="text-xs opacity-70">
            {manage_gateway_set_members_order_hint()}
          </p>

          <div role="list" class="flex flex-col gap-2">
            {#each members as member, idx (member.profile_id)}
              <GatewayMemberCard
                {member}
                position={idx + 1}
                dragOver={dragOver === idx}
                isFirst={idx === 0}
                isLast={idx === members.length - 1}
                editing={editingId === member.profile_id}
                editDraft={edits[member.profile_id] ?? null}
                {busy}
                onDragStart={() => (dragFrom = idx)}
                onDragEnd={clearDrag}
                onDragOver={() => (dragOver = idx)}
                onDrop={() => {
                  if (dragFrom !== null) reorder(dragFrom, idx);
                  clearDrag();
                }}
                onRemove={() => removeAt(idx)}
                onMoveUp={() => step(idx, -1)}
                onMoveDown={() => step(idx, 1)}
                onToggleEdit={() => toggleEdit(member)}
                onEditDraft={(next) => applyEdit(member.profile_id, next)}
              />
            {/each}
          </div>

          {#if error !== null}
            <p class="text-sm" role="alert">{error}</p>
          {/if}

          <div class="flex gap-2">
            <button
              type="button"
              class="btn flex-1 preset-outlined-surface-500 hover:preset-filled-surface-500"
              disabled={busy}
              onclick={onCancel}
            >
              {common_cancel()}
            </button>
            <button
              type="submit"
              class="btn flex-1 preset-filled-primary-500 text-on-primary-500 transition hover:brightness-110"
              disabled={!canSubmit}
            >
              {common_save()}
            </button>
          </div>
        </form>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
