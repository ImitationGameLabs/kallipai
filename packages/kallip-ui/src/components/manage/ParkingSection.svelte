<script lang="ts">
  // Parking pool for the profiles page: profiles held out of rotation, same
  // card language as the sets above, but the container stays dashed (not a
  // rotation slot) and its add button opens the single-profile
  // ProfileDialog. Drag state and drop mutations stay in the page.
  import type { SvelteMap } from "svelte/reactivity";
  import { Plus } from "@lucide/svelte";
  import type {
    ProfileModel,
    ProfileModelProbeReport,
  } from "@kallipai/kallip-client";
  import {
    manage_profiles_parking,
    manage_profiles_parking_add,
    manage_profiles_parking_desc_l1,
    manage_profiles_parking_desc_l2,
  } from "../../paraglide/messages.js";
  import ProfileCard from "./ProfileCard.svelte";

  let {
    parking,
    reports,
    isProbing,
    dragOverParking,
    onCardDragStart,
    onCardDragEnd,
    onParkingDragOver,
    onParkingDragLeave,
    onParkingDrop,
    onTest,
    onEdit,
    onAdd,
    onRemove,
  }: {
    parking: readonly ProfileModel[];
    reports: SvelteMap<string, ProfileModelProbeReport>;
    isProbing: boolean;
    dragOverParking: boolean;
    onCardDragStart: (fromIdx: number, id: string, e: DragEvent) => void;
    onCardDragEnd: () => void;
    onParkingDragOver: () => void;
    onParkingDragLeave: () => void;
    onParkingDrop: () => void;
    onTest: (idx: number) => void;
    onEdit: (idx: number) => void;
    onAdd: () => void;
    onRemove: (idx: number, profileId: string) => void;
  } = $props();
</script>

<section class="space-y-3">
  <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
    {manage_profiles_parking()}
  </h2>
  <div class="text-xs opacity-60 mt-1 space-y-0.5">
    <p>{manage_profiles_parking_desc_l1()}</p>
    <p>{manage_profiles_parking_desc_l2()}</p>
  </div>
  <div
    role="list"
    class="card preset-tonal-surface border-2 border-dashed border-surface-400 p-4 space-y-3 {dragOverParking
      ? 'outline-2 outline-dashed outline-primary-500'
      : ''}"
    ondragover={(e) => {
      e.preventDefault();
      onParkingDragOver();
    }}
    ondragleave={onParkingDragLeave}
    ondrop={(e) => {
      e.preventDefault();
      onParkingDrop();
    }}
  >
    {#each parking as profile, idx (idx)}
      {@const report = reports.get(`p:${profile.id}`)}
      <ProfileCard
        {profile}
        {report}
        {isProbing}
        onDragStart={(e) => onCardDragStart(idx, profile.id, e)}
        onDragEnd={onCardDragEnd}
        onTest={() => onTest(idx)}
        onEdit={() => onEdit(idx)}
        onRemove={() => onRemove(idx, profile.id)}
      />
    {/each}
    <button
      type="button"
      class="card preset-tonal-surface border-2 border-dashed border-surface-400 p-4 flex items-center justify-center gap-2 min-h-24 w-full hover:preset-filled-surface-100-900 transition cursor-pointer"
      onclick={onAdd}
    >
      <Plus class="size-6 opacity-70" />
      <span class="text-sm opacity-70">
        {manage_profiles_parking_add()}
      </span>
    </button>
  </div>
</section>
