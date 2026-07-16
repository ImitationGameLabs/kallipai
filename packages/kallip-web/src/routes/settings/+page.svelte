<script lang="ts">
  import {
    loadConfig,
    saveConfig,
    clearConfig,
    type DirectConfig,
  } from "$lib/config/credentials";
  import { sessionStore } from "$lib/session/session.svelte";
  import { connectDirect } from "$lib/session/connect";

  const existing = loadConfig();
  let daemonUrl = $state(
    existing?.backend === "direct"
      ? existing.daemonUrl
      : "http://127.0.0.1:3000",
  );
  let authToken = $state(
    existing?.backend === "direct" ? existing.authToken : "",
  );
  let error = $state<string | null>(null);
  let connecting = $state(false);

  function validUrl(value: string): boolean {
    try {
      const url = new URL(value);
      return url.protocol === "http:" || url.protocol === "https:";
    } catch {
      return false;
    }
  }

  async function connect() {
    error = null;
    if (!validUrl(daemonUrl.trim())) {
      error = "Daemon URL must be a valid http(s) URL.";
      return;
    }
    connecting = true;
    const config: DirectConfig = {
      backend: "direct",
      daemonUrl: daemonUrl.trim(),
      authToken,
    };
    try {
      const session = await connectDirect(config);
      saveConfig(config);
      await sessionStore.attach(session);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      connecting = false;
    }
  }

  function disconnect() {
    sessionStore.detach();
  }

  function clearSaved() {
    clearConfig();
    sessionStore.detach();
  }
</script>

<svelte:head><title>KallipAI · settings</title></svelte:head>

<div class="h-full overflow-y-auto">
  <div class="p-6 max-w-md space-y-6">
    <h1 class="text-xl font-semibold">Connection</h1>

    {#if sessionStore.connected}
      <div class="card preset-tonal-surface p-4 space-y-3">
        <div class="flex items-center gap-2 text-sm">
          <span class="size-2 rounded-full bg-success-500" aria-hidden="true"
          ></span>
          <span class="font-medium">Connected</span>
        </div>
        <div class="text-xs opacity-60 font-mono break-all">{daemonUrl}</div>
        <div class="flex gap-2">
          <a href="/" class="btn btn-sm preset-filled-primary-500">Open chat</a>
          <button class="btn btn-sm preset-tonal-surface" onclick={disconnect}
            >Disconnect</button
          >
        </div>
      </div>
    {/if}

    <form
      class="space-y-4"
      onsubmit={(e) => {
        e.preventDefault();
        void connect();
      }}
    >
      <label class="block space-y-1">
        <span class="text-sm opacity-70">Daemon URL</span>
        <input
          class="input"
          bind:value={daemonUrl}
          placeholder="http://127.0.0.1:3000"
        />
        <span class="block text-xs opacity-50"
          >Base URL of the kallip daemon HTTP API.</span
        >
      </label>

      <label class="block space-y-1">
        <span class="text-sm opacity-70">Auth token</span>
        <input
          class="input"
          type="password"
          bind:value={authToken}
          placeholder="sk-operator-…"
        />
        <span class="block text-xs opacity-50"
          >Operator token accepted by the daemon.</span
        >
      </label>

      {#if error}
        <div class="text-sm text-error-500">{error}</div>
      {/if}

      <button
        class="btn preset-filled-primary-500"
        disabled={connecting || !authToken.trim()}
      >
        {connecting ? "Connecting…" : "Connect"}
      </button>
    </form>

    <div class="border-t border-surface-200 pt-4">
      <button class="btn btn-sm preset-tonal-error" onclick={clearSaved}>
        Clear saved config
      </button>
    </div>
  </div>
</div>
