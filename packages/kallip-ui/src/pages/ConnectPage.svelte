<script lang="ts">
  import { sessionStore } from "../lib/session/session.svelte";
  import { agoraSession } from "../lib/session/agora.svelte";
  import { connectDirect } from "../lib/session/connect.ts";
  import { configStore } from "../lib/config/config.svelte";
  import type { OfflineModeConfig } from "../lib/config/config.ts";
  import { navigate } from "../lib/shell/port.ts";
  import { classifyError } from "../lib/errors.ts";
  import Brand from "../components/Brand.svelte";
  import Banner from "../components/Banner.svelte";

  let daemonUrl = $state("http://127.0.0.1:3000");
  let authToken = $state("");
  // Field-level validation (e.g. malformed URL); shown inline.
  let error = $state<string | null>(null);
  // Raw connection failure from connectDirect; classified into the banner so
  // internal paths (e.g. `daemon request failed: /agents`) are never shown.
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
      daemonUrl = cfg.offline.daemonUrl;
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
    if (!validUrl(daemonUrl.trim())) {
      error = "Daemon URL must be a valid http(s) URL.";
      return;
    }
    connecting = true;
    const config: OfflineModeConfig = {
      daemonUrl: daemonUrl.trim(),
      authToken,
    };
    try {
      const session = await connectDirect(config);
      await configStore.setOffline(config);
      await configStore.setActiveMode("offline");
      await sessionStore.attach(session);
    } catch (e) {
      // Full error (with cause chain) to the console; the banner shows only the
      // classified, path-free message.
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

<svelte:head><title>KallipAI · offline connect</title></svelte:head>

{#if connectView}
  <!-- Floats over the centered form; the URL-validation hint stays inline. -->
  <Banner
    floating
    title={connectView.title}
    detail={connectView.detail}
    hint={connectView.hint}
  />
{/if}

<div class="flex items-center justify-center min-h-dvh p-4 bg-surface-200">
  <form
    class="w-full max-w-sm space-y-6 p-6 bg-surface-100 border border-surface-200 shadow-sm rounded-xl"
    onsubmit={submit}
  >
    <div class="text-center space-y-1">
      <Brand size="lg" />
      <p class="text-sm opacity-60">Connect an offline daemon</p>
    </div>

    <label class="block space-y-1">
      <span class="text-sm opacity-70">
        Daemon URL <span class="text-error-500">*</span>
      </span>
      <input
        class="input"
        autocomplete="url"
        bind:value={daemonUrl}
        placeholder="http://127.0.0.1:3000"
        required
      />
      <span class="block text-xs opacity-50"
        >Base URL of the kallip daemon HTTP API.</span
      >
    </label>

    <label class="block space-y-1">
      <span class="text-sm opacity-70">
        Auth token <span class="text-error-500">*</span>
      </span>
      <input
        class="input"
        type="text"
        autocomplete="off"
        bind:value={authToken}
        placeholder="sk-operator-…"
        required
      />
      <span class="block text-xs opacity-50"
        >Operator token accepted by the daemon.</span
      >
    </label>

    {#if error}
      <p role="alert" class="text-sm text-error-500">{error}</p>
    {/if}

    <button
      type="submit"
      class="btn preset-filled-primary-500 w-full"
      disabled={connecting || !authToken.trim()}
    >
      {connecting ? "Connecting…" : "Connect"}
    </button>

    <p class="text-center text-sm">
      <button
        type="button"
        onclick={useOnline}
        class="font-medium text-primary-500 hover:underline cursor-pointer"
        >Online mode</button
      >
    </p>
  </form>
</div>
