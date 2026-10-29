<script lang="ts">
  // Unassigned pool for the profiles page: profiles held out of rotation, same
  // card language as the sets above, but the container stays dashed (not a
  // rotation slot) and its add button opens the single-profile
  // ProfileDialog. Drag state and drop mutations stay in the page.
  import type { SvelteMap } from "svelte/reactivity";
  import { Plus } from "@lucide/svelte";
  import type {
    ProfileModel,
    ProfileModelProbeReport,
  } from "@kallipai/kallipai-client";
  import {
    manage_profiles_unassigned,
    manage_profiles_unassigned_add,
    manage_profiles_unassigned_desc_l1,
    manage_profiles_unassigned_desc_l2,
  } from "../../paraglide/messages.js";
  import ProfileCard from "./ProfileCard.svelte";

  let {
    unassigned,
    reports,
    isProbing,
    dragOverUnassigned,
    onCardDragStart,
    onCardDragEnd,
    onUnassignedDragOver,
    onUnassignedDragLeave,
    onUnassignedDrop,
    onTest,
    onEdit,
    onAdd,
    onRemove,
    readOnly = false,
  }: {
    unassigned: readonly ProfileModel[];
    reports: SvelteMap<string, ProfileModelProbeReport>;
    isProbing: boolean;
    dragOverUnassigned: boolean;
    onCardDragStart: (fromIdx: number, id: string, e: DragEvent) => void;
    onCardDragEnd: () => void;
    onUnassignedDragOver: () => void;
    onUnassignedDragLeave: () => void;
    onUnassignedDrop: () => void;
    onTest: (idx: number) => void;
    onEdit: (idx: number) => void;
    onAdd: () => void;
    onRemove: (idx: number, profileId: string) => void;
    readOnly?: boolean;
  } = $props();
</script>

<section class="space-y-3">
  <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
    {manage_profiles_unassigned()}
  </h2>
  <div class="text-xs opacity-60 mt-1 space-y-0.5">
    <p>{manage_profiles_unassigned_desc_l1()}</p>
    <p>{manage_profiles_unassigned_desc_l2()}</p>
  </div>
  <div
    role="list"
    class="card preset-tonal-surface border-2 border-dashed border-surface-400 p-4 space-y-3 {dragOverUnassigned
      ? 'outline-2 outline-dashed outline-primary-500'
      : ''}"
    ondragover={(e) => {
      if (readOnly) return;
      e.preventDefault();
      onUnassignedDragOver();
    }}
    ondragleave={() => {
      if (readOnly) return;
      onUnassignedDragLeave();
    }}
    ondrop={(e) => {
      if (readOnly) return;
      e.preventDefault();
      onUnassignedDrop();
    }}
  >
    {#each unassigned as profile, idx (idx)}
      {@const report = reports.get(`p:${profile.id}`)}
      <ProfileCard
        {profile}
        {report}
        {isProbing}
        {readOnly}
        onDragStart={(e) => onCardDragStart(idx, profile.id, e)}
        onDragEnd={onCardDragEnd}
        onTest={() => onTest(idx)}
        onEdit={() => {
          if (!readOnly) onEdit(idx);
        }}
        onRemove={() => {
          if (!readOnly) onRemove(idx, profile.id);
        }}
      />
    {/each}
    <button
      type="button"
      class="card preset-tonal-surface border-2 border-dashed border-surface-400 p-4 flex items-center justify-center gap-2 min-h-24 w-full hover:preset-filled-surface-100-900 transition cursor-pointer"
      onclick={onAdd}
      disabled={readOnly}
    >
      <Plus class="size-6 opacity-70" />
      <span class="text-sm opacity-70">
        {manage_profiles_unassigned_add()}
      </span>
    </button>
  </div>
</section>
