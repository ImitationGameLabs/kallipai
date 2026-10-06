<script lang="ts">
  import { MENU_ITEM, MENU_ITEM_DANGER } from "../../lib/classes.ts";
  // Providers pool for the profiles page: one card per endpoint plus the
  // dashed add-provider card. Probe actions and dialog requests cross as
  // events; the report chips read the shared per-provider probe map.
  import type { SvelteMap } from "svelte/reactivity";
  import { Menu } from "@skeletonlabs/skeleton-svelte";
  import ActionMenu from "../ActionMenu.svelte";
  import { FlaskConical, Pencil, Plus, Trash } from "@lucide/svelte";
  import type {
    ProfileProvider,
    ProfileProviderProbeReport,
  } from "@kallipai/kallipai-client";
  import {
    modelsCountLabel,
    probeStatusColor,
    probeStatusLabel,
  } from "../../lib/manage/profiles-view.ts";
  import {
    common_edit,
    manage_profiles_add_provider,
    common_remove,
    manage_profiles_provider_base_url_default,
    manage_profiles_provider_card_base_url_label,
    manage_profiles_profile_provider_label,
    manage_profiles_provider_actions_aria,
    manage_profiles_providers,
    manage_profiles_test,
  } from "../../paraglide/messages.js";

  let {
    providers,
    reports,
    isProbing,
    onTest,
    onEdit,
    onRemove,
    onAdd,
    readOnly = false,
  }: {
    providers: ProfileProvider[];
    reports: SvelteMap<string, ProfileProviderProbeReport>;
    isProbing: boolean;
    onTest: (id: string) => void;
    onEdit: (provider: ProfileProvider) => void;
    onRemove: (provider: ProfileProvider) => void;
    onAdd: () => void;
    readOnly?: boolean;
  } = $props();
</script>

<section class="space-y-3">
  <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
    {manage_profiles_providers()}
  </h2>
  <div class="grid gap-3 sm:grid-cols-2">
    {#each providers as ep (ep.id)}
      {@const report = reports.get(ep.id)}
      <div class="card preset-tonal-surface p-4 space-y-2 min-w-0">
        <div class="flex items-center justify-between gap-2">
          <span class="font-mono text-sm font-semibold truncate min-w-0 flex-1"
            >{ep.id}</span
          >
          <ActionMenu
            label={manage_profiles_provider_actions_aria()}
            disabled={isProbing || readOnly}
            onSelect={(value) => {
              if (value === "test") onTest(ep.id);
              else if (value === "edit" && !readOnly) onEdit(ep);
              else if (value === "remove" && !readOnly) onRemove(ep);
            }}
          >
            <Menu.Item value="test" class={MENU_ITEM}>
              <FlaskConical class="size-4" />
              {manage_profiles_test()}
            </Menu.Item>
            <Menu.Item value="edit" class={MENU_ITEM}>
              <Pencil class="size-4" />
              {common_edit()}
            </Menu.Item>
            <Menu.Separator class="my-1 border-t border-surface-300" />
            <Menu.Item value="remove" class={MENU_ITEM_DANGER}>
              <Trash class="size-4" />
              {common_remove()}
            </Menu.Item>
          </ActionMenu>
        </div>
        <dl class="text-xs space-y-1">
          <div class="flex gap-2">
            <dt class="opacity-60 shrink-0">
              {manage_profiles_profile_provider_label()}:
            </dt>
            <dd class="font-mono min-w-0">{ep.family}</dd>
          </div>
          <div class="flex gap-2">
            <dt class="opacity-60 shrink-0">
              {manage_profiles_provider_card_base_url_label()}:
            </dt>
            <dd class="font-mono truncate min-w-0">
              {ep.base_url ?? manage_profiles_provider_base_url_default()}
            </dd>
          </div>
          <div class="flex gap-2">
            <dt class="opacity-60 shrink-0">API key:</dt>
            <dd class="font-mono min-w-0 break-all">{ep.api_key}</dd>
          </div>
        </dl>
        {#if report}
          <div class="border-t border-surface-300 pt-2 text-xs space-y-1">
            <span class={probeStatusColor[report.status]}>
              {probeStatusLabel(report.status)}
            </span>
            {#if report.latency_ms != null}
              <span class="opacity-60 ml-2">{report.latency_ms}ms</span>
            {/if}
            {#if report.catalog_count != null}
              <span class="opacity-60 ml-2">
                {modelsCountLabel(report.catalog_count)}
              </span>
            {/if}
            {#if report.detail}
              <p class="opacity-60 font-mono break-all">
                {report.detail}
              </p>
            {/if}
          </div>
        {/if}
      </div>
    {/each}
    <button
      type="button"
      class="card preset-tonal-surface border-2 border-dashed border-surface-400 p-4 flex items-center justify-center gap-2 min-h-24 hover:preset-filled-surface-100-900 transition cursor-pointer"
      onclick={onAdd}
      disabled={readOnly}
    >
      <Plus class="size-6 opacity-70" />
      <span class="text-sm opacity-70">
        {manage_profiles_add_provider()}
      </span>
    </button>
  </div>
</section>
