<script lang="ts">
  // The unassigned drop's fallback: when an unassigned profile is dropped
  // on the console while no collection detail is open, this dialog takes
  // the placement (a collection pick, then a member-set pick). Both
  // selects are required; a collection without sets disables the confirm
  // with a hint instead of a dead submit. An unassigned profile belongs
  // to one collection, so a drop that names its own collection pins the
  // first pick: the server would refuse a membership across collections.
  import { Dialog } from "@skeletonlabs/skeleton-svelte";
  import FormDialog from "../../FormDialog.svelte";
  import ReqMark from "../../ReqMark.svelte";
  import type {
    AdminCollectionRow,
    AdminSetRow,
  } from "../../../lib/manage/gateway/client.ts";
  import { memberSetsOf } from "../../../lib/gateway/collections.ts";
  import {
    manage_gateway_placement_collection_label,
    manage_gateway_placement_empty,
    manage_gateway_placement_profile_label,
    manage_gateway_placement_set_label,
    manage_gateway_placement_title,
  } from "../../../paraglide/messages.js";

  let {
    open,
    profileId,
    collections,
    sets,
    sourceCollection = undefined,
    busy = false,
    error = null,
    onConfirm,
    onCancel,
  }: {
    open: boolean;
    /** The unassigned profile being placed (carried from the drop). */
    profileId: string | null;
    /** The catalog's collections, in list order. */
    collections: readonly AdminCollectionRow[];
    /** The catalog's sets: the member picks come from here. */
    sets: readonly AdminSetRow[];
    /** The dragged profile's collection: pins the collection pick. */
    sourceCollection?: string;
    busy?: boolean;
    error?: string | null;
    /** The chosen member set. The destination is the profile's own
     * collection (the pick above is pinned to it), so the set name is the
     * whole answer the caller needs. */
    onConfirm: (setName: string) => void;
    onCancel: () => void;
  } = $props();

  let collectionDraft = $state("");
  let setNameDraft = $state("");

  function reseed(): void {
    collectionDraft = sourceCollection ?? collections[0]?.name ?? "";
    setNameDraft = "";
  }

  // One collection's sets can take this profile; the others would be
  // refused, so the pick offers only them when the drop named one.
  const pickedCollections = $derived(
    sourceCollection === undefined
      ? collections
      : collections.filter((c) => c.name === sourceCollection),
  );

  const targetCollection = $derived(
    collections.find((c) => c.name === collectionDraft) ?? null,
  );

  const memberSets = $derived.by(() =>
    targetCollection === null ? [] : memberSetsOf(sets ?? [], targetCollection),
  );

  const canSubmit = $derived(
    collectionDraft !== "" && setNameDraft !== "" && memberSets.length > 0,
  );

  function submit(): void {
    onConfirm(setNameDraft);
  }
</script>

<FormDialog
  {open}
  title={manage_gateway_placement_title()}
  size="md"
  {busy}
  {error}
  {canSubmit}
  onOpen={reseed}
  onSubmit={submit}
  {onCancel}
>
  {#if profileId !== null}
    <Dialog.Description class="text-sm opacity-70 font-mono">
      {manage_gateway_placement_profile_label({ name: profileId })}
    </Dialog.Description>
  {/if}

  <label class="flex flex-col gap-1">
    <span class="text-xs font-medium">
      {manage_gateway_placement_collection_label()}
      <ReqMark />
    </span>
    <select
      class="input text-sm font-mono"
      bind:value={collectionDraft}
      onchange={() => (setNameDraft = "")}
      disabled={busy || sourceCollection !== undefined}
      aria-required="true"
    >
      {#each pickedCollections as collection (collection.name)}
        <option value={collection.name}>{collection.name}</option>
      {/each}
    </select>
  </label>

  <label class="flex flex-col gap-1">
    <span class="text-xs font-medium">
      {manage_gateway_placement_set_label()}
      <ReqMark />
    </span>
    <select
      class="input text-sm font-mono"
      bind:value={setNameDraft}
      disabled={busy || memberSets.length === 0}
      aria-required="true"
    >
      {#if setNameDraft === ""}
        <option value="" disabled></option>
      {/if}
      {#each memberSets as set (set.name)}
        <option value={set.name}>{set.name}</option>
      {/each}
    </select>
  </label>
  {#if memberSets.length === 0}
    <p class="text-xs opacity-60">
      {manage_gateway_placement_empty()}
    </p>
  {/if}
</FormDialog>
