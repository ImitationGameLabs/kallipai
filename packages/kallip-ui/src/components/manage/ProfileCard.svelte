<script lang="ts">
  // One draggable profile card (set slots and the parking area share this
  // language). The card only reports the raw drag lifecycle and menu
  // intents; payloads and mutations stay with the owning page/section.
  import { Menu, Portal } from "@skeletonlabs/skeleton-svelte";
  import { FlaskConical, MoreVertical, Pencil, Trash } from "@lucide/svelte";
  import type {
    ProfileModel,
    ProfileModelProbeReport,
  } from "@kallipai/kallip-client";
  import { TONAL_ICON_SURF } from "../../lib/classes.ts";
  import {
    probeStatusColor,
    probeStatusLabel,
  } from "../../lib/manage/profiles-view.ts";
  import {
    common_edit,
    common_remove,
    manage_profiles_max_context_label,
    manage_profiles_profile_actions_aria,
    manage_profiles_profile_effort_label,
    manage_profiles_profile_model_label,
    manage_profiles_profile_provider_label,
    manage_profiles_test,
  } from "../../paraglide/messages.js";

  let {
    profile,
    report,
    isProbing,
    onDragStart,
    onDragEnd,
    onTest,
    onEdit,
    onRemove,
  }: {
    profile: ProfileModel;
    report?: ProfileModelProbeReport;
    isProbing: boolean;
    onDragStart: (e: DragEvent) => void;
    onDragEnd: () => void;
    onTest: () => void;
    onEdit: () => void;
    onRemove: (() => void) | null;
  } = $props();
</script>

<div
  role="listitem"
  class="card preset-filled-surface-100-900 p-3 space-y-1 cursor-grab"
  draggable="true"
  ondragstart={(e) => {
    // Firefox only starts a drag session if dataTransfer gets data.
    if (e.dataTransfer) {
      e.dataTransfer.setData("text/plain", profile.id);
      e.dataTransfer.effectAllowed = "move";
    }
    onDragStart(e);
  }}
  ondragend={onDragEnd}
>
  <div class="flex items-center justify-between gap-2">
    <span class="font-mono text-sm">{profile.id}</span>
    <Menu
      positioning={{ placement: "bottom-end" }}
      onSelect={(e) => {
        if (e.value === "test") onTest();
        else if (e.value === "edit") onEdit();
        else if (e.value === "remove" && onRemove) onRemove();
      }}
    >
      <Menu.Trigger
        class="size-10 {TONAL_ICON_SURF} shrink-0"
        aria-label={manage_profiles_profile_actions_aria()}
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
              {manage_profiles_test()}
            </Menu.Item>
            <Menu.Item
              value="edit"
              class="flex items-center gap-2 px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
            >
              <Pencil class="size-4" />
              {common_edit()}
            </Menu.Item>
            {#if onRemove}
              <Menu.Separator class="my-1 border-t border-surface-300" />
              <Menu.Item
                value="remove"
                class="flex items-center gap-2 px-3 py-2 rounded-base text-sm text-error-500 dark:text-error-400 cursor-pointer hover:preset-filled-error-500"
              >
                <Trash class="size-4" />
                {common_remove()}
              </Menu.Item>
            {/if}
          </Menu.Content>
        </Menu.Positioner>
      </Portal>
    </Menu>
  </div>
  <dl class="text-xs space-y-0.5">
    <div class="flex gap-2">
      <dt class="opacity-60">
        {manage_profiles_profile_provider_label()}:
      </dt>
      <dd class="font-mono">{profile.endpoint}</dd>
    </div>
    <div class="flex gap-2">
      <dt class="opacity-60">
        {manage_profiles_profile_model_label()}:
      </dt>
      <dd class="font-mono">{profile.model}</dd>
    </div>
    <div class="flex gap-2">
      <dt class="opacity-60">
        {manage_profiles_max_context_label()}:
      </dt>
      <dd class="font-mono">{profile.max_context_window}</dd>
    </div>
    {#if profile.effort}
      <div class="flex gap-2">
        <dt class="opacity-60">
          {manage_profiles_profile_effort_label()}:
        </dt>
        <dd class="font-mono">{profile.effort}</dd>
      </div>
    {/if}
  </dl>
  {#if report}
    <div class="text-xs">
      <span class={probeStatusColor[report.status]}>
        {probeStatusLabel(report.status)}
      </span>
      {#if report.detail}
        <span class="opacity-60 ml-2 font-mono break-all">
          {report.detail}
        </span>
      {/if}
    </div>
  {/if}
</div>
