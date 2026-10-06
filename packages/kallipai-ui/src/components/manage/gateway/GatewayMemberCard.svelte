<script lang="ts">
  // One draggable gateway member card inside the catalog set dialog.
  // Like ProfileCard, the card only reports the raw drag lifecycle;
  // the dialog owns the order draft and the drop mutation. The field
  // set is the gateway wire's AdminProfileRow (profile_id / family /
  // model / parked / owner) -- the overlap with ProfileCard is id and
  // model only, so the variant stands alone rather than polluting
  // both domains. Labels stay the wire's field names (mono): the card
  // is an admin registry row, not a translated form.
  import type { AdminProfileRow } from "../../../lib/manage/gateway/client.ts";
  import { Trash } from "@lucide/svelte";
  import { manage_profiles_set_dialog_remove_member } from "../../../paraglide/messages.js";

  let {
    member,
    position,
    dragOver = false,
    onDragStart,
    onDragEnd,
    onDragOver,
    onDrop,
    onRemove,
  }: {
    member: AdminProfileRow;
    /** 1-based display position (the order is the failover sequence). */
    position: number;
    /** Another card is hovering over this one (drop indicator). */
    dragOver?: boolean;
    onDragStart: (e: DragEvent) => void;
    onDragEnd: () => void;
    onDragOver: (e: DragEvent) => void;
    onDrop: () => void;
    /** Remove this member from the set (the dialog mutates the draft). */
    onRemove: () => void;
  } = $props();
</script>

<div
  role="listitem"
  class="card preset-filled-surface-100-900 p-3 space-y-1 cursor-grab {dragOver
    ? 'outline-2 outline-dashed outline-primary-500'
    : ''}"
  draggable="true"
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
    {#if member.parked}
      <span class="badge preset-tonal-surface text-xs shrink-0 font-mono">
        parked
      </span>
    {/if}
    <button
      type="button"
      class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-error-500 shrink-0"
      aria-label={manage_profiles_set_dialog_remove_member()}
      onclick={onRemove}
    >
      <Trash class="size-4" />
    </button>
  </div>
  <dl class="text-xs space-y-0.5">
    <div class="flex gap-2">
      <dt class="opacity-60">family:</dt>
      <dd class="font-mono truncate">{member.family}</dd>
    </div>
    <div class="flex gap-2">
      <dt class="opacity-60">model:</dt>
      <dd class="font-mono truncate">{member.model}</dd>
    </div>
    <div class="flex gap-2">
      <dt class="opacity-60">owner:</dt>
      <dd class="font-mono truncate">{member.owner}</dd>
    </div>
  </dl>
</div>
