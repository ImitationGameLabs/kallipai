<script lang="ts">
  // A public tagma profile card, keyed by tagma_id (carried on every room agent
  // sender). Backed by the unauthenticated `GET /v1/tagmata/{id}/profile`
  // endpoint (minimal disclosure: label + owner + created_at; never the pinned
  // key or flags). Reached by clicking an agent sender's header in a room. A
  // protected app-shell route (logged-out deep links redirect to /login?next=).
  //
  // The "Message" CTA opens the tagma's DM, but ONLY for a tagma the caller owns
  // (the bilateral DM is owner-scoped); a peer's tagma has no CTA.
  import { ChevronLeft, Cpu, MessageSquare } from "@lucide/svelte";
  import { agoraClientOrFail, agoraSession } from "../lib/session/agora.svelte";
  import { navigate } from "../lib/shell/port.ts";
  import { formatDateTime } from "../lib/tagmata.svelte.ts";
  import { TONAL_ICON_SURF } from "../lib/classes.ts";
  import type { PublicTagmaProfile } from "@kallipai/kallip-agora-client";
  import {
    common_loading,
    common_back_aria,
    tagma_fallback_label,
    tagma_profile_subtitle,
    tagma_profile_unnamed,
    tagma_profile_created,
    tagma_profile_message,
  } from "../paraglide/messages.js";

  let { tagmaId }: { tagmaId: string } = $props();

  let profile = $state<PublicTagmaProfile | null>(null);
  let loading = $state(true);
  let error = $state<string | null>(null);

  // Refetch on id change (SvelteKit reuses this page across [id] changes). The
  // stale guard drops a late prior response so a quick id->id nav cannot
  // overwrite the current view (mirrors RoomSettingsPage.svelte).
  $effect(() => {
    const id = tagmaId;
    loading = true;
    error = null;
    profile = null;
    let stale = false;
    void agoraClientOrFail()
      .getTagmaProfile(id)
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

  // The DM CTA appears only for an ENROLLED tagma the caller owns (the bilateral
  // DM resolves via the owner-scoped registry and a peer's tagma -- or a pending
  // own one -- would fail to open). Mirrors the `state === "enrolled"` filter
  // used by RootLayout's presence sink.
  const ownTagma = $derived(
    agoraSession.tagmata.some(
      (t) => t.tagma_id === tagmaId && t.state === "enrolled",
    ),
  );

  function back(): void {
    if (history.length > 1) history.back();
    else navigate("/tagmata");
  }
</script>

<svelte:head><title>KallipAI · {profile?.label ?? "tagma"}</title></svelte:head>

<div class="flex flex-col h-full">
  <header
    class="px-4 pb-2 pt-[max(0.5rem,env(safe-area-inset-top))] border-b border-surface-200-800 grid grid-cols-[auto_1fr_auto] items-center gap-2 md:flex"
  >
    <button
      type="button"
      class="size-8 {TONAL_ICON_SURF} shrink-0"
      aria-label={common_back_aria()}
      onclick={back}
    >
      <ChevronLeft class="size-4" />
    </button>
    <div
      class="flex flex-col min-w-0 justify-center px-2 text-center md:flex-1 md:px-0 md:text-left"
    >
      <p class="text-sm font-semibold truncate">
        {profile?.label ??
          (profile
            ? tagma_fallback_label({ id: tagmaId.slice(0, 8) })
            : "tagma")}
      </p>
      <p class="text-xs opacity-50 truncate">{tagma_profile_subtitle()}</p>
    </div>
    <!-- Balances the back button so the title stays optically centred below md. -->
    <span class="size-8 md:hidden" aria-hidden="true"></span>
  </header>

  <div class="flex-1 min-h-0 overflow-auto">
    <div class="mx-auto w-full max-w-2xl p-4 flex flex-col gap-3">
      {#if loading}
        <p class="text-sm opacity-60">{common_loading()}</p>
      {:else if error}
        <p class="text-sm text-error-500 dark:text-error-400">{error}</p>
      {:else if profile}
        <section
          class="card preset-tonal-surface p-4 flex flex-col gap-1 text-sm"
        >
          <div class="flex items-center gap-2">
            <Cpu class="size-4 shrink-0 opacity-70" aria-hidden="true" />
            <span class="text-base font-medium truncate">
              {profile.label ?? tagma_profile_unnamed()}
            </span>
          </div>
          <div class="text-xs opacity-60 font-mono break-all">
            @{profile.owner_username}
          </div>
          {#if profile.owner_display_name}
            <div class="text-xs opacity-60 truncate">
              {profile.owner_display_name}
            </div>
          {/if}
          <div class="text-xs opacity-50">
            {tagma_profile_created({
              date: formatDateTime(profile.created_at),
            })}
          </div>
        </section>
        {#if ownTagma}
          <button
            type="button"
            class="btn btn-sm preset-outlined-surface-500 self-start flex items-center gap-2"
            onclick={() => navigate(`/chat/t/${tagmaId}`)}
          >
            <MessageSquare class="size-4" />
            {tagma_profile_message()}
          </button>
        {/if}
      {/if}
    </div>
  </div>
</div>
