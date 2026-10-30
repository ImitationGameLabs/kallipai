<script lang="ts">
  // One draggable gateway member card inside a set dialog: the order
  // controls, the remove action, and (when the dialog opens it) the inline
  // form for this row's fields. Like ProfileCard, the card owns no draft of
  // its own: the dialog holds the order and the edit map, and the card
  // reports every change back. The row is the shared DraftRow: the fields
  // the form edits plus the read-only facts a face may carry (family,
  // owner), each shown only when that face's row has it. The read
  // row keeps the wire's terms in mono; the form fields use the shared
  // profile copy (model, context window, effort, modalities).
  import {
    contextProblem,
    modelProblem,
    type DraftRow,
    type MemberEdit,
  } from "../../lib/manage/gateway/setDraft.ts";
  import { EFFORT_OPTIONS } from "../../lib/manage/compute.ts";
  import type { ReasoningEffort } from "@kallipai/kallipai-client";
  import { ArrowDown, ArrowUp, Pencil, Trash } from "@lucide/svelte";
  import ModalityTags from "../ModalityTags.svelte";
  import {
    manage_gateway_set_member_edit,
    manage_gateway_set_member_edit_done,
    manage_gateway_set_member_move_down,
    manage_gateway_set_member_move_up,
    manage_profiles_profile_dialog_invalid_window,
    manage_profiles_profile_dialog_invalid_model,
    manage_profiles_set_dialog_remove_member,
    user_gateway_profile_context,
    user_gateway_profile_effort,
    user_gateway_profile_effort_unset,
    user_gateway_profile_modalities,
    user_gateway_profile_model,
  } from "../../paraglide/messages.js";

  let {
    member,
    position,
    dragOver = false,
    isFirst = false,
    isLast = false,
    editing = false,
    editDraft = null,
    busy = false,
    editable = true,
    onDragStart,
    onDragEnd,
    onDragOver,
    onDrop,
    onRemove,
    onMoveUp,
    onMoveDown,
    onToggleEdit,
    onEditDraft,
  }: {
    member: DraftRow;
    /** 1-based display position (the order is the failover sequence). */
    position: number;
    /** Another card is hovering over this one (drop indicator). */
    dragOver?: boolean;
    /** Order edges: the step buttons are disabled, not hidden. */
    isFirst?: boolean;
    isLast?: boolean;
    /** The dialog has this row's form open. */
    editing?: boolean;
    /** The row's editable projection while the form is open. */
    editDraft?: MemberEdit | null;
    busy?: boolean;
    /** Whether this face edits the row's fields at all: a create has no
     * registry row to PUT, so that dialog offers no form. */
    editable?: boolean;
    onDragStart: (e: DragEvent) => void;
    onDragEnd: () => void;
    onDragOver: (e: DragEvent) => void;
    onDrop: () => void;
    /** Remove this member from the set (the dialog mutates the draft). */
    onRemove: () => void;
    onMoveUp: () => void;
    onMoveDown: () => void;
    onToggleEdit: () => void;
    onEditDraft: (next: MemberEdit) => void;
  } = $props();

  const modalities = $derived(editDraft?.modalities ?? []);

  // A save in flight already carries the snapshot it read from this draft,
  // so a field typed now would be dropped when the dialog closes or
  // reseeds: the form freezes with the rest of the card.
  function patch(next: Partial<MemberEdit>): void {
    if (editDraft === null || busy) return;
    onEditDraft({ ...editDraft, ...next });
  }
</script>

<div
  role="listitem"
  class="card preset-filled-surface-100-900 p-3 space-y-1 {editing || busy
    ? ''
    : 'cursor-grab'} {dragOver
    ? 'outline-2 outline-dashed outline-primary-500'
    : ''}"
  draggable={!editing && !busy}
  ondragstart={(e) => {
    // Firefox only starts a drag session if dataTransfer gets data.
    if (e.dataTransfer) {
      e.dataTransfer.setData("text/plain", member.profile_id);
      e.dataTransfer.effectAllowed = "move";
    }
    onDragStart(e);
  }}
  ondragend={onDragEnd}
  ondragover={(e) => {
    e.preventDefault();
    onDragOver(e);
  }}
  ondrop={(e) => {
    e.preventDefault();
    onDrop();
  }}
>
  <div class="flex items-center justify-between gap-2">
    <span class="font-mono text-sm truncate">
      {position}. {member.profile_id}
    </span>
    <div class="flex items-center gap-1 shrink-0">
      <button
        type="button"
        class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
        aria-label={manage_gateway_set_member_move_up()}
        disabled={isFirst || busy}
        onclick={onMoveUp}
      >
        <ArrowUp class="size-4" />
      </button>
      <button
        type="button"
        class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
        aria-label={manage_gateway_set_member_move_down()}
        disabled={isLast || busy}
        onclick={onMoveDown}
      >
        <ArrowDown class="size-4" />
      </button>
      {#if editable}
        <button
          type="button"
          class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-primary-500"
          aria-label={editing
            ? manage_gateway_set_member_edit_done()
            : manage_gateway_set_member_edit()}
          aria-pressed={editing}
          disabled={busy}
          onclick={onToggleEdit}
        >
          <Pencil class="size-4" />
        </button>
      {/if}
      <button
        type="button"
        class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-error-500"
        aria-label={manage_profiles_set_dialog_remove_member()}
        disabled={busy}
        onclick={onRemove}
      >
        <Trash class="size-4" />
      </button>
    </div>
  </div>
  {#if editable && editing && editDraft !== null}
    <div class="flex flex-col gap-2 pt-1">
      <label class="flex flex-col gap-1">
        <span class="text-xs font-medium">{user_gateway_profile_model()}</span>
        <input
          class="input text-sm font-mono"
          value={editDraft.model}
          disabled={busy}
          oninput={(e) => patch({ model: e.currentTarget.value })}
        />
        {#if modelProblem(editDraft)}
          <span class="text-xs text-error-500 dark:text-error-400"
            >{manage_profiles_profile_dialog_invalid_model()}</span
          >
        {/if}
      </label>
      <label class="flex flex-col gap-1">
        <span class="text-xs font-medium">{user_gateway_profile_context()}</span
        >
        <input
          class="input text-sm"
          inputmode="numeric"
          value={editDraft.context_window}
          disabled={busy}
          oninput={(e) => patch({ context_window: e.currentTarget.value })}
        />
        {#if contextProblem(editDraft)}
          <span class="text-xs text-error-500 dark:text-error-400"
            >{manage_profiles_profile_dialog_invalid_window()}</span
          >
        {/if}
      </label>
      <label class="flex flex-col gap-1">
        <span class="text-xs font-medium">{user_gateway_profile_effort()}</span>
        <select
          class="select text-sm"
          value={editDraft.effort ?? ""}
          disabled={busy}
          onchange={(e) =>
            patch({
              effort:
                e.currentTarget.value === "" ? null : e.currentTarget.value,
            })}
        >
          {#if editDraft.effort !== null && !EFFORT_OPTIONS.includes(editDraft.effort as ReasoningEffort)}
            <!-- A stored row can carry an effort outside the enum: keep it
            selectable so an edit never silently rewrites it. -->
            <option value={editDraft.effort}>{editDraft.effort}</option>
          {/if}
          <option value="">{user_gateway_profile_effort_unset()}</option>
          {#each EFFORT_OPTIONS as e (e)}
            <option value={e}>{e}</option>
          {/each}
        </select>
      </label>
      <ModalityTags
        label={user_gateway_profile_modalities()}
        selected={modalities}
        disabled={busy}
        onChange={(next) => patch({ modalities: [...next] })}
      />
    </div>
  {/if}
  <dl class="text-xs space-y-0.5">
    <div class="flex gap-2">
      <dt class="opacity-60">family:</dt>
      <dd class="font-mono truncate">{member.family}</dd>
    </div>
    <div class="flex gap-2">
      <dt class="opacity-60">model:</dt>
      <dd class="font-mono truncate">{member.model}</dd>
    </div>
    {#if member.owner !== undefined}
      <div class="flex gap-2">
        <dt class="opacity-60">owner:</dt>
        <dd class="font-mono truncate">{member.owner}</dd>
      </div>
    {/if}
  </dl>
</div>
