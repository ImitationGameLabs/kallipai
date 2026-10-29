<script lang="ts">
  // The user face's set dialog: the description, the member rows with their
  // order (the failover sequence), each row's fields as an inline form, and
  // a picker over the collection's own profiles for adding a member. Create
  // mode adds the name field and starts with an empty list, which the server
  // takes as a legal membership. The draft rules live in the shared set-draft
  // lib; this component owns the state and the layout.
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import ReqMark from "../ReqMark.svelte";
  import GatewayMemberCard from "./GatewayMemberCard.svelte";
  import {
    changedUpdates,
    firstOffending,
    memberEditOf,
    memberIds,
    moveMember,
    reseed,
    setsAlsoHolding,
    type DraftRow,
    type MemberEdit,
  } from "../../lib/manage/gateway/setDraft.ts";
  import {
    diffBase,
    draftRowOf,
    poolOf,
    siblingSets,
    type UserSetDetail,
    type UserSetSaveDraft,
  } from "../../lib/gateway/userSetDraft.ts";
  import type { UserProfileRow, UserSetRow } from "../../lib/gateway/client.ts";
  import {
    common_add,
    common_cancel,
    common_save,
    manage_gateway_set_members_order_hint,
    user_gateway_set_add,
    user_gateway_set_add_member,
    user_gateway_set_add_member_empty,
    user_gateway_set_description,
    user_gateway_set_dialog_collection_label,
    user_gateway_set_edit_title,
    user_gateway_set_name,
  } from "../../paraglide/messages.js";

  let {
    open,
    set,
    collection,
    profiles = null,
    siblings = [],
    busy = false,
    error = null,
    landed = [],
    onSave,
    onCancel,
  }: {
    open: boolean;
    /** The set under edit (null = create, inside `collection`). */
    set: UserSetDetail | null;
    /** The collection a created set lands in (pre-bound by the section). */
    collection: string;
    /** The collection's own profile rows: the pool a member comes from,
     * and the rows a member id resolves to. Null means the page has not
     * read them yet: rows unread are not rows known to be absent, so the
     * picker stays quiet rather than claiming the pool is empty. */
    profiles?: readonly UserProfileRow[] | null;
    /** The collection's sets: names the other sets a member also sits in. */
    siblings?: readonly UserSetRow[];
    /** A save is in flight: the footer and the cards are locked. */
    busy?: boolean;
    /** The server's reason for the last failed save (the dialog stays). */
    error?: string | null;
    /** The rows whose PUT landed before a save stopped: a re-read reseeds
     * these only, so a row that never reached the registry keeps its draft. */
    landed?: readonly string[];
    onSave: (result: UserSetSaveDraft) => void;
    onCancel: () => void;
  } = $props();

  // The local draft: the name (create only), the description, the member rows
  // (rows, not ids, so the cards keep rendering during a reorder), the
  // per-row field edits, and which row's form is open. The draft seeds from
  // the set the page hands; a re-read after a failed save goes through
  // setDraft's reseed, which decides what stays a draft.
  let nameDraft = $state("");
  let descriptionDraft = $state("");
  let members = $state<DraftRow[]>([]);
  let edits = $state<Record<string, MemberEdit>>({});
  let editingId = $state<string | null>(null);
  let pickedId = $state("");
  let dragFrom = $state<number | null>(null);
  let dragOver = $state<number | null>(null);
  let seeded: UserSetDetail | null = null;
  let seededKey: string | null = null;
  // The member ids the draft was seeded with; a re-seed hands back the seed
  // it just read from the registry, so the next one compares against that.
  // What the user added after a seed has no registry row yet, and a
  // re-read must not take it back out.
  let seededIds = $state<readonly string[]>([]);

  const CREATE_KEY = "new set";

  $effect(() => {
    if (!open) {
      // A closed dialog seeds from scratch when it opens again, even on the
      // same set: what it held was a draft of the last visit.
      seeded = null;
      seededKey = null;
      return;
    }
    if (set === null) {
      // Create mode has no registry row to come back to, so the page never
      // re-hands it: the empty draft is seeded once per open.
      if (seededKey === CREATE_KEY) return;
      seededKey = CREATE_KEY;
      nameDraft = "";
      descriptionDraft = "";
      members = [];
      edits = {};
      editingId = null;
      pickedId = "";
      seededIds = [];
      dragFrom = null;
      dragOver = null;
      return;
    }
    if (set === seeded) return;
    seeded = set;
    const key = `${set.collection_name}/${set.name}`;
    const fresh = key !== seededKey;
    seededKey = key;
    if (fresh) {
      nameDraft = "";
      descriptionDraft = set.description;
      members = [...set.profiles];
      edits = {};
      editingId = null;
      pickedId = "";
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

  // The picker's pool, and which other sets carry each member: both are
  // the lib's calls, so the rules are tested without a browser. An unread
  // row set offers nothing to add.
  const pool = $derived(profiles === null ? [] : poolOf(profiles, members));

  // Which other sets of the collection carry each member: an edit to a row
  // lands in every set that holds it, and the card says which.
  const alsoIn = $derived(
    set === null
      ? {}
      : setsAlsoHolding(
          { name: set.name, profiles: members },
          siblingSets(siblings),
        ),
  );

  // A save in flight greys the button; create also needs a name. An empty
  // member list is a legal membership, so it does not grey anything: the
  // PATCH carries members: [] and clears the set.
  const canSubmit = $derived(
    !busy && (set !== null || nameDraft.trim() !== ""),
  );

  function clearDrag(): void {
    dragFrom = null;
    dragOver = null;
  }

  function reorder(fromIdx: number, toIdx: number): void {
    // A save in flight freezes the order: the PATCH already carries the
    // sequence the draft had when it left.
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

  // Removal joins the draft: the row leaves the list here, and the membership
  // change lands on save. Its field edits leave with it: the member is
  // leaving the set, so those values have no row left to land on.
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

  // Adding a member is a membership change too, so the order is the user's
  // from here on: a re-read must not take the row back out.
  function addMember(): void {
    if (busy || pickedId === "") return;
    const row = profiles?.find((profile) => profile.profile_id === pickedId);
    if (row === undefined) return;
    members = [...members, draftRowOf(row)];
    pickedId = "";
  }

  // The form draft is seeded from the row the first time it opens, so a
  // toggle back and forth keeps what was typed.
  function toggleEdit(member: DraftRow): void {
    // A create has no registry row to PUT: the form is edit-only, and the
    // card hides its pen. The gate here is what makes that a rule, not a
    // hidden button.
    if (busy || set === null) return;
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

  // A drafted field the server would refuse stops the save instead of riding
  // along or being dropped with its form: the first offending row opens, so
  // the problem shows under the field that holds it.
  function submit(): void {
    if (!canSubmit) return;
    const offending = firstOffending(members, edits);
    if (offending !== undefined) {
      editingId = offending.profile_id;
      return;
    }
    // A create has no registry rows, so it sends no updates at all: the
    // create request carries the member list and nothing here has a row to
    // PUT. The card closes its form in create mode too, but that gate
    // protects the editing path; this ternary is what keeps a create from
    // ever carrying a PUT body.
    onSave({
      name: set ? set.name : nameDraft.trim(),
      description: descriptionDraft,
      members: memberIds(members),
      updates:
        set === null
          ? []
          : changedUpdates(diffBase(set.profiles, members), members, edits),
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
          {set ? user_gateway_set_edit_title() : user_gateway_set_add()}
          {#if set}
            <span class="font-mono opacity-80">{set.name}</span>
          {/if}
        </Dialog.Title>

        <form
          class="flex flex-col gap-4"
          onsubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          {#if set === null}
            <p class="text-xs opacity-60">
              {user_gateway_set_dialog_collection_label()}:
              <span class="font-mono">{collection}</span>
            </p>
            <label class="flex flex-col gap-1">
              <span class="text-xs font-medium">
                {user_gateway_set_name()}
                <ReqMark />
              </span>
              <input
                class="input text-sm font-mono"
                bind:value={nameDraft}
                aria-required="true"
              />
            </label>
          {/if}
          <label class="flex flex-col gap-1">
            <span class="text-xs font-medium">
              {user_gateway_set_description()}
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
                alsoIn={alsoIn[member.profile_id] ?? []}
                showParked={false}
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
                editable={set !== null}
                onToggleEdit={() => toggleEdit(member)}
                onEditDraft={(next) => applyEdit(member.profile_id, next)}
              />
            {/each}
          </div>

          <div class="flex flex-col gap-2">
            <span class="text-xs font-medium">
              {user_gateway_set_add_member()}
            </span>
            {#if profiles !== null && pool.length === 0}
              <p class="text-xs opacity-70">
                {user_gateway_set_add_member_empty()}
              </p>
            {:else}
              <div class="flex gap-2">
                <select
                  class="select text-sm font-mono flex-1"
                  bind:value={pickedId}
                  aria-label={user_gateway_set_add_member()}
                >
                  <option value="">…</option>
                  {#each pool as row (row.profile_id)}
                    <option value={row.profile_id}>{row.profile_id}</option>
                  {/each}
                </select>
                <button
                  type="button"
                  class="btn preset-outlined-surface-500 hover:preset-filled-surface-500"
                  disabled={busy || pickedId === ""}
                  onclick={addMember}
                >
                  {common_add()}
                </button>
              </div>
            {/if}
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
