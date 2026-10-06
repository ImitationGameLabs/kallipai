<script lang="ts">
  // The parking drop's fallback: when a parked profile is dropped on
  // the console while no collection detail is open, this dialog takes
  // the placement (a collection pick, then a member-set pick). Both
  // selects are required; a collection without sets disables the
  // confirm with a hint instead of a dead submit.
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import ReqMark from "../../ReqMark.svelte";
  import type {
    AdminCollectionRow,
    AdminSetRow,
  } from "../../../lib/manage/gateway/client.ts";
  import { memberSetsOf } from "../../../lib/gateway/collections.ts";
  import {
    common_cancel,
    common_save,
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
    busy = false,
    error = null,
    onConfirm,
    onCancel,
  }: {
    open: boolean;
    /** The parked profile being placed (carried from the drop). */
    profileId: string | null;
    /** The catalog's collections, in list order. */
    collections: readonly AdminCollectionRow[];
    /** The catalog's sets: the member picks come from here. */
    sets: readonly AdminSetRow[];
    busy?: boolean;
    error?: string | null;
    onConfirm: (collection: string, setName: string) => void;
    onCancel: () => void;
  } = $props();

  let collectionDraft = $state("");
  let setDraft = $state("");
  let lastOpen = false;
  $effect(() => {
    if (open && !lastOpen) {
      collectionDraft = collections[0]?.name ?? "";
      setDraft = "";
    }
    lastOpen = open;
  });

  const memberSets = $derived.by(() =>
    memberSetsOf(
      sets ?? [],
      collections.find((c) => c.name === collectionDraft)?.sets ?? [],
    ),
  );

  const canSubmit = $derived(
    collectionDraft !== "" && setDraft !== "" && memberSets.length > 0,
  );

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open && !busy) onCancel();
  }

  function submit(): void {
    if (!canSubmit || busy) return;
    onConfirm(collectionDraft, setDraft);
  }
</script>

<Dialog {open} {onOpenChange}>
  <Portal>
    <Dialog.Backdrop class="fixed inset-0 bg-surface-50-950/60 z-50" />
    <Dialog.Positioner class="fixed inset-0 z-50 grid place-items-center p-4">
      <Dialog.Content
        class="card preset-tonal-surface w-full max-w-md p-6 flex flex-col gap-4"
      >
        <Dialog.Title class="text-lg font-semibold">
          {manage_gateway_placement_title()}
        </Dialog.Title>
        {#if profileId !== null}
          <Dialog.Description class="text-sm opacity-70 font-mono">
            {manage_gateway_placement_profile_label({ name: profileId })}
          </Dialog.Description>
        {/if}

        <form
          class="flex flex-col gap-3"
          onsubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          <label class="flex flex-col gap-1">
            <span class="text-xs font-medium">
              {manage_gateway_placement_collection_label()}
              <ReqMark />
            </span>
            <select
              class="input text-sm font-mono"
              bind:value={collectionDraft}
              onchange={() => (setDraft = "")}
              disabled={busy}
              aria-required="true"
            >
              {#each collections as collection (collection.name)}
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
              bind:value={setDraft}
              disabled={busy || memberSets.length === 0}
              aria-required="true"
            >
              {#if setDraft === ""}
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

          {#if error}
            <p class="text-sm text-error-500" role="alert">{error}</p>
          {/if}

          <div class="flex gap-2">
            <button
              type="button"
              class="btn flex-1 preset-outlined-surface-500 hover:preset-filled-surface-500"
              onclick={onCancel}
            >
              {common_cancel()}
            </button>
            <button
              type="submit"
              class="btn flex-1 preset-filled-primary-500 text-on-primary-500 transition hover:brightness-110"
              disabled={!canSubmit || busy}
            >
              {common_save()}
            </button>
          </div>
        </form>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
