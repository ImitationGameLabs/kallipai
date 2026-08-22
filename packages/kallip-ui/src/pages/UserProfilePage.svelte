<script lang="ts">
  // A public user profile card, keyed by username (the `@handle` a room sender
  // carries). Backed by the unauthenticated `GET /v1/users/{username}` endpoint
  // (minimal disclosure: display name + created_at; never email/user_id). Reached
  // by clicking a human sender's header in a room. A protected app-shell route:
  // a logged-out deep link redirects to /login?next=, then returns here.
  import { ChevronLeft } from "@lucide/svelte";
  import { agoraClientOrFail } from "../lib/session/agora.svelte";
  import { navigate } from "../lib/shell/port.ts";
  import { formatDateTime } from "../lib/tagmata.svelte.ts";
  import { TONAL_ICON_SURF } from "../lib/classes.ts";
  import type { PublicUserProfile } from "@kallipai/kallip-agora-client";
  import {
    common_loading,
    common_back_aria,
    user_profile_subtitle,
    user_profile_joined,
  } from "../paraglide/messages.js";

  let { handle }: { handle: string } = $props();

  // Refetch on handle change (SvelteKit reuses this page across [handle] param
  // changes without remount, mirroring RoomSettingsPage).
  let profile = $state<PublicUserProfile | null>(null);
  let loading = $state(true);
  let error = $state<string | null>(null);

  $effect(() => {
    const username = handle;
    loading = true;
    error = null;
    profile = null;
    // Stale guard: SvelteKit reuses this page across [handle] changes, so a
    // quick alice -> bob nav can leave two fetches in flight. Drop the late
    // (prior) response so it cannot overwrite the current view (mirrors
    // RoomSettingsPage.svelte's `stale` flag).
    let stale = false;
    void agoraClientOrFail()
      .getUserProfile(username)
      .then((p) => {
        if (!stale) {
          profile = p;
          loading = false;
        }
      })
      .catch((e) => {
        if (!stale) {
          error = e instanceof Error ? e.message : String(e);
          loading = false;
        }
      });
    return () => {
      stale = true;
    };
  });

  function back(): void {
    if (history.length > 1) history.back();
    else navigate("/tagmata");
  }
</script>

<svelte:head><title>KallipAI · {handle}</title></svelte:head>

<div class="flex flex-col h-full">
  <header
    class="px-4 py-2 border-b border-surface-200-800 flex items-center gap-2"
  >
    <button
      type="button"
      class="size-8 {TONAL_ICON_SURF} shrink-0"
      aria-label={common_back_aria()}
      onclick={back}
    >
      <ChevronLeft class="size-4" />
    </button>
    <div class="flex flex-col min-w-0 flex-1">
      <p class="text-sm font-semibold truncate">@{handle}</p>
      <p class="text-xs opacity-50 truncate">{user_profile_subtitle()}</p>
    </div>
  </header>

  <div class="flex-1 min-h-0 overflow-auto">
    <div class="mx-auto w-full max-w-2xl p-4">
      {#if loading}
        <p class="text-sm opacity-60">{common_loading()}</p>
      {:else if error}
        <p class="text-sm text-error-500 dark:text-error-400">{error}</p>
      {:else if profile}
        <section
          class="card preset-tonal-surface p-4 flex flex-col gap-1 text-sm"
        >
          <div class="text-base font-medium truncate">
            {profile.display_name ?? profile.username}
          </div>
          <div class="text-xs opacity-60 font-mono break-all">
            @{profile.username}
          </div>
          <div class="text-xs opacity-50">
            {user_profile_joined({ date: formatDateTime(profile.created_at) })}
          </div>
        </section>
      {/if}
    </div>
  </div>
</div>
