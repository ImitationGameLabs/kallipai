<script lang="ts">
  import { agoraSession } from "../lib/session/agora.svelte";
  import { channelsStore } from "../lib/session/channels.svelte";
  import { connectDirect } from "../lib/session/connect.ts";
  import { configStore } from "../lib/config/config.svelte";
  import type { OfflineModeConfig } from "../lib/config/config.ts";
  import { navigate } from "../lib/shell/port.ts";
  import { classifyError } from "../lib/errors.ts";
  import Brand from "../components/Brand.svelte";
  import FormError from "../components/FormError.svelte";
  import {
    connect_title,
    connect_subtitle,
    connect_url_label,
    connect_url_hint,
    connect_token_label,
    connect_token_hint,
    connect_url_invalid,
    connect_connecting,
    connect_submit,
    connect_online_mode,
  } from "../paraglide/messages.js";

  let tagmaUrl = $state("http://127.0.0.1:3000");
  let authToken = $state("");
  // Field-level validation (e.g. malformed URL); shown inline.
  let error = $state<string | null>(null);
  // Raw connection failure from connectDirect; classified into the inline
  // FormError so internal paths (e.g. `tagma request failed: /agents`) never
  // reach the user.
  let connectError = $state<unknown>(null);
  const connectView = $derived(
    connectError === null ? null : classifyError(connectError),
  );
  let connecting = $state(false);

  // Seed from retained offline creds once they load -- e.g. a returning user
  // whose boot reconnect failed and landed back here, or an online user
  // re-entering offline setup. One-shot via `seeded`.
  let seeded = $state(false);
  $effect(() => {
    const cfg = configStore.value;
    if (!seeded && cfg?.offline) {
      tagmaUrl = cfg.offline.tagmaUrl;
      authToken = cfg.offline.authToken;
      seeded = true;
    }
  });

  function validUrl(value: string): boolean {
    try {
      const url = new URL(value);
      return url.protocol === "http:" || url.protocol === "https:";
    } catch {
      return false;
    }
  }

  // On success the gate (offline + /connect + connected) redirects to "/" -- so
  // this page does NOT navigate. Single owner of the post-connect route.
  // Entering offline mode no longer touches the online (agora) session: its
  // cookie survives so a later switch back is re-auth-free. The retained offline
  // creds are persisted via setOffline, then the active mode flips to offline.
  async function submit(e: Event) {
    e.preventDefault();
    error = null;
    connectError = null;
    if (!validUrl(tagmaUrl.trim())) {
      error = connect_url_invalid();
      return;
    }
    connecting = true;
    const config: OfflineModeConfig = {
      tagmaUrl: tagmaUrl.trim(),
      authToken,
    };
    try {
      const { transport, conversationId } = await connectDirect(config);
      await configStore.setOffline(config);
      await configStore.setActiveMode("offline");
      await channelsStore.attachLocal(transport, conversationId);
    } catch (e) {
      // Full error (with cause chain) to the console; the inline FormError
      // shows only the classified, path-free message.
      console.error(e);
      connectError = e;
    } finally {
      connecting = false;
    }
  }

  // Abandon offline setup and head back to the online mode: flip the active
  // mode (retaining offline creds for next time), re-resolve the agora user,
  // then navigate. The explicit navigate is load-bearing -- the gate renders
  // /connect for everyone in online mode, so without it the user would stay on
  // this now-mismatched page.
  async function useOnline() {
    await configStore.setActiveMode("online");
    void agoraSession.whoami();
    await navigate(agoraSession.user ? "/tagmata" : "/login");
  }
</script>

<svelte:head><title>{connect_title()}</title></svelte:head>


<div class="flex items-center justify-center min-h-dvh p-4 bg-surface-200-800">
  <form
    class="w-full max-w-sm space-y-6 p-6 bg-surface-100-900 border border-surface-200-800 shadow-sm rounded-xl"
    onsubmit={submit}
  >
    <div class="text-center space-y-1">
      <Brand size="lg" />
      <p class="text-sm opacity-60">{connect_subtitle()}</p>
    </div>

    <label class="block space-y-1">
      <span class="text-sm opacity-70">
        {connect_url_label()}
        <span class="text-error-500 dark:text-error-400">*</span>
      </span>
      <input
        class="input"
        autocomplete="url"
        bind:value={tagmaUrl}
        placeholder="http://127.0.0.1:3000"
        required
      />
      <span class="block text-xs opacity-50">{connect_url_hint()}</span>
    </label>

    <label class="block space-y-1">
      <span class="text-sm opacity-70">
        {connect_token_label()}
        <span class="text-error-500 dark:text-error-400">*</span>
      </span>
      <input
        class="input"
        type="text"
        autocomplete="off"
        bind:value={authToken}
        placeholder="sk-operator-…"
        required
      />
      <span class="block text-xs opacity-50">{connect_token_hint()}</span>
    </label>

    {#if connectView}
      <FormError message={connectView.title} detail={connectView.detail} hint={connectView.hint} />
    {/if}
    {#if error}
      <FormError message={error} />
    {/if}

    <button
      type="submit"
      class="btn preset-filled-primary-500 w-full"
      disabled={connecting || !authToken.trim()}
    >
      {connecting ? connect_connecting() : connect_submit()}
    </button>

    <p class="text-center text-sm">
      <button
        type="button"
        onclick={useOnline}
        class="font-medium text-primary-500 dark:text-primary-400 hover:underline cursor-pointer"
        >{connect_online_mode()}</button
      >
    </p>
  </form>
</div>
