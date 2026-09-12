<script lang="ts">
  // Sets pool for the profiles page: one draggable-profile container per
  // named set (name / description / default badge header, the set probe
  // footer, and the dashed add-set card). Drag state and drop mutations
  // stay in the page; this section only reports the raw drag lifecycle
  // (start/over/leave/end/drop) so both drop targets share one source of
  // truth.
  import type { SvelteMap } from "svelte/reactivity";
  import { Menu, Portal } from "@skeletonlabs/skeleton-svelte";
  import {
    FlaskConical,
    MoreVertical,
    Pencil,
    Plus,
    Star,
    Trash,
  } from "@lucide/svelte";
  import type {
    ProfileModel,
    ProfileModelProbeReport,
    ProfileSet,
  } from "@kallipai/kallip-client";
  import { TONAL_ICON_SURF } from "../../lib/classes.ts";
  import ProfileCard from "./ProfileCard.svelte";
  import {
    profileKey,
    probeStatusColor,
    probeStatusLabel,
  } from "../../lib/manage/profiles-view.ts";
  import {
    formatModalities,
    setEffectiveModalities,
    setHasShadowedMembers,
  } from "../../lib/manage/compute.ts";
  import {
    common_edit,
    common_remove,
    manage_profiles_add_set,
    manage_profiles_max_context_label,
    manage_profiles_profile_actions_aria,
    manage_profiles_profile_model_label,
    manage_profiles_profile_provider_label,
    manage_profiles_probe_set_fail,
    manage_profiles_probe_set_ok,
    manage_profiles_set_actions_aria,
    manage_profiles_set_as_default,
    manage_profiles_set_default_badge,
    manage_profiles_set_drop_here,
    manage_profiles_set_modalities_label,
    manage_profiles_set_modalities_shadowed,
    manage_profiles_sets,
    manage_profiles_sets_desc_l1,
    manage_profiles_sets_desc_l2,
    manage_profiles_sets_desc_l3,
    manage_profiles_sets_desc_l4,
    manage_profiles_test,
    manage_profiles_test_all,
  } from "../../paraglide/messages.js";

  let {
    sets,
    defaultName,
    reports,
    isProbing,
    dragOverSet,
    onCardDragStart,
    onCardDragEnd,
    onSetDragOver,
    onSetDragLeave,
    onSetDrop,
    onTestSet,
    onTestProfile,
    onEditSet,
    onEditProfile,
    onRemoveSet,
    onSetDefault,
    onAddSet,
    onRemoveProfile,
  }: {
    /** Draft sets as name/value pairs (Object.entries order). */
    sets: readonly [string, ProfileSet][];
    /** The draft's default set name (badge + menu state), if any. */
    defaultName: string | undefined;
    reports: SvelteMap<string, ProfileModelProbeReport>;
    isProbing: boolean;
    dragOverSet: string | null;
    onCardDragStart: (
      fromSet: string,
      fromIdx: number,
      id: string,
      e: DragEvent,
    ) => void;
    onCardDragEnd: () => void;
    onSetDragOver: (setName: string) => void;
    onSetDragLeave: (setName: string) => void;
    onSetDrop: (setName: string) => void;
    onTestSet: (setName: string) => void;
    onTestProfile: (setName: string, profileIdx: number) => void;
    onEditSet: (setName: string) => void;
    onEditProfile: (setName: string, profileIdx: number) => void;
    onRemoveSet: (setName: string) => void;
    onSetDefault: (setName: string) => void;
    onAddSet: () => void;
    onRemoveProfile: (
      setName: string,
      profileIdx: number,
      profileId: string,
    ) => void;
  } = $props();
</script>

<section class="space-y-3">
  <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
    {manage_profiles_sets()}
  </h2>
  <div class="text-xs opacity-60 mt-1 space-y-0.5">
    <p>{manage_profiles_sets_desc_l1()}</p>
    <p>{manage_profiles_sets_desc_l2()}</p>
    <p>{manage_profiles_sets_desc_l3()}</p>
    <p>{manage_profiles_sets_desc_l4()}</p>
  </div>

  {#each sets as [setName, set] (setName)}
    {@const setReport = [...reports.entries()]
      .filter(([k]) => k.startsWith(`${setName}:`))
      .map(([, v]) => v)}
    <div
      role="list"
      class="card preset-tonal-surface p-4 space-y-3 {dragOverSet === setName
        ? 'outline-2 outline-dashed outline-primary-500'
        : ''}"
      ondragover={(e) => {
        e.preventDefault();
        onSetDragOver(setName);
      }}
      ondragleave={() => onSetDragLeave(setName)}
      ondrop={(e) => {
        e.preventDefault();
        onSetDrop(setName);
      }}
    >
      <div class="flex items-center justify-between gap-2">
        <div class="min-w-0">
          <div class="flex items-center gap-2">
            <span class="text-sm font-medium font-mono truncate">{setName}</span
            >
            {#if defaultName === setName}
              <span class="badge preset-filled-primary-500 text-xs shrink-0">
                {manage_profiles_set_default_badge()}
              </span>
            {/if}
          </div>
          {#if set.description}
            <p class="text-xs opacity-60 mt-0.5 truncate">{set.description}</p>
          {/if}
          {#if set.profiles.length > 0}
            <p class="text-xs opacity-60 mt-0.5 truncate">
              {manage_profiles_set_modalities_label()}:
              {formatModalities(setEffectiveModalities(set))}
              {#if setHasShadowedMembers(set)}
                <span
                  class="text-warning-500"
                  title={manage_profiles_set_modalities_shadowed()}>!</span
                >
              {/if}
            </p>
          {/if}
        </div>
        <Menu
          positioning={{ placement: "bottom-end" }}
          onSelect={(e) => {
            if (e.value === "test") onTestSet(setName);
            else if (e.value === "default") onSetDefault(setName);
            else if (e.value === "edit") onEditSet(setName);
            else if (e.value === "remove") onRemoveSet(setName);
          }}
        >
          <Menu.Trigger
            class="size-10 {TONAL_ICON_SURF} shrink-0"
            aria-label={manage_profiles_set_actions_aria()}
            disabled={isProbing}
          >
            <MoreVertical class="size-4" />
          </Menu.Trigger>
          <Portal>
            <Menu.Positioner>
              <Menu.Content class="card preset-tonal-surface p-1 min-w-[8rem]">
                <Menu.Item
                  value="test"
                  class="flex items-center gap-2 px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
                >
                  <FlaskConical class="size-4" />
                  {manage_profiles_test_all()}
                </Menu.Item>
                <Menu.Item
                  value="default"
                  disabled={defaultName === setName}
                  class="flex items-center gap-2 px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500 data-[disabled]:cursor-not-allowed"
                >
                  <Star class="size-4" />
                  {manage_profiles_set_as_default()}
                </Menu.Item>
                <Menu.Item
                  value="edit"
                  class="flex items-center gap-2 px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
                >
                  <Pencil class="size-4" />
                  {common_edit()}
                </Menu.Item>
                <Menu.Item
                  value="remove"
                  class="flex items-center gap-2 px-3 py-2 rounded-base text-sm text-error-500 dark:text-error-400 cursor-pointer hover:preset-filled-error-500"
                >
                  <Trash class="size-4" />
                  {common_remove()}
                </Menu.Item>
              </Menu.Content>
            </Menu.Positioner>
          </Portal>
        </Menu>
      </div>

      {#each set.profiles as profile, profileIdx (profileIdx)}
        {@const report = reports.get(profileKey(setName, profile.id))}
        <ProfileCard
          {profile}
          {report}
          {isProbing}
          onDragStart={(e: DragEvent) =>
            onCardDragStart(setName, profileIdx, profile.id, e)}
          onDragEnd={onCardDragEnd}
          onTest={() => onTestProfile(setName, profileIdx)}
          onEdit={() => onEditProfile(setName, profileIdx)}
          onRemove={() => onRemoveProfile(setName, profileIdx, profile.id)}
        />
      {/each}
      {#if set.profiles.length === 0}
        <p class="text-xs opacity-50">
          {manage_profiles_set_drop_here()}
        </p>
      {/if}

      <!-- Card footer: the set probe summary (a result lands beside
           the kebab menu that produced it). -->
      {#if setReport.length > 0}
        <div class="flex items-center gap-2 flex-wrap text-xs">
          {#if setReport.every((r) => r.status === "ok")}
            <span class={probeStatusColor.ok}>
              {manage_profiles_probe_set_ok()}
            </span>
          {:else}
            <span class={probeStatusColor.invalid_config}>
              {manage_profiles_probe_set_fail()}
              {setReport
                .filter((r) => r.status !== "ok")
                .map((r) => r.profile_id)
                .join(", ")}
            </span>
          {/if}
        </div>
      {/if}
    </div>
  {/each}

  <!-- Add-set card: same level as the set containers; appends an
       empty set directly (no dialog) — drag profiles in or use the
       set's Edit. An empty set cannot be saved (PUT rejects), by
       design: fill it before saving. -->
  <button
    type="button"
    class="card preset-tonal-surface border-2 border-dashed border-surface-400 p-4 flex items-center justify-center gap-2 min-h-24 w-full hover:preset-filled-surface-100-900 transition cursor-pointer"
    onclick={onAddSet}
  >
    <Plus class="size-6 opacity-70" />
    <span class="text-sm opacity-70">
      {manage_profiles_add_set()}
    </span>
  </button>
</section>
