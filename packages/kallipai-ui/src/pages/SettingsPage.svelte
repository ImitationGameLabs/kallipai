<script lang="ts">
  import { channelsStore } from "../lib/session/channels.svelte";
  import { configStore } from "../lib/config/config.svelte";
  import { shellMode } from "../lib/shell/port.ts";
  import { managementBackend } from "../lib/manage/client.ts";
  import { KallipaiError } from "@kallipai/kallipai-common";
  import {
    timezoneLoad,
    timezoneSetting,
    saveTimezoneSetting,
    timezoneCandidates,
  } from "../lib/time/stamp.svelte.ts";
  import LightSwitch from "../components/LightSwitch.svelte";
  import LanguageSwitch from "../components/LanguageSwitch.svelte";
  import { Switch } from "@skeletonlabs/skeleton-svelte";
  import {
    notificationPermission,
    requestNotificationPermission,
    type PermissionState,
  } from "../lib/session/notify.ts";
  import {
    settings_appearance,
    settings_dark_mode,
    settings_language,
    settings_timezone,
    settings_timezone_desc,
    settings_timezone_placeholder,
    settings_timezone_saved,
    settings_timezone_clear,
    settings_timezone_invalid,
    settings_title,
    settings_heading,
    settings_connection,
    settings_connected,
    settings_disconnected,
    settings_disconnect,
    settings_reconnect,
    error_unknown,
    chat_notifications_label,
    common_save,
    chat_notifications_desc,
    chat_notifications_denied,
  } from "../paraglide/messages.js";

  // This page answers one question: how the product behaves on this machine.
  // Identity, passkeys, linked providers and access tokens live on /account;
  // the offline tagma connection stays here -- Disconnect/Reconnect is tagma
  // session management, not an account action.
  const mode = $derived(shellMode());
  const offlineUrl = $derived(configStore.value?.offline?.tagmaUrl ?? "");

  // -- notifications (all hosts; one web+tauri switch) ------
  // The toggle click IS the user gesture: enabling requests permission right
  // here (MDN: browsers drop prompt-less requests), and only a granted
  // answer persists the switch. Denied is a browser-level terminal state --
  // the guidance line (rendered proactively, not only after a failed
  // request) points at the browser's site settings, the only way back.
  const notificationsEnabled = $derived(
    configStore.value?.notificationsEnabled === true,
  );
  let notifyPermission = $state<PermissionState>("default");
  $effect(() => {
    void notificationPermission().then((s) => (notifyPermission = s));
  });
  async function onNotificationsToggle(event: { checked: boolean }) {
    if (!event.checked) {
      await configStore.setNotificationsEnabled(false);
      return;
    }
    notifyPermission = await requestNotificationPermission();
    if (notifyPermission === "granted") {
      await configStore.setNotificationsEnabled(true);
    }
  }
  // Offline: drop the tagma session without abandoning offline mode.
  function disconnect() {
    channelsStore.detachLocal();
  }

  // -- timezone (offline management face) ---------------------------------
  // The tagma-level display timezone: what every absolute stamp renders in.
  // The server validates IANA names (400 on a miss); the input is free text
  // with a browser-provided candidate list, so no zone catalogue ships in
  // the bundle.
  let tzSeeded = $state(false);
  let tzInput = $state("");
  const tzCandidates = timezoneCandidates();
  let tzBusy = $state(false);
  let tzFeedback = $state<"saved" | "invalid" | "network" | null>(null);

  $effect(() => {
    if (mode !== "offline" || !timezoneLoad.done || !timezoneLoad.ok) return;
    if (!tzSeeded) {
      tzInput = timezoneSetting.value ?? "";
      tzSeeded = true;
    }
  });
  async function onTimezoneSave(): Promise<void> {
    const name = tzInput.trim();
    tzBusy = true;
    try {
      const confirmed = await saveTimezoneSetting(
        (tz) => managementBackend().putTimezone(tz),
        name === "" ? null : name,
      );
      tzInput = confirmed ?? "";
      tzFeedback = "saved";
    } catch (e) {
      tzFeedback =
        e instanceof KallipaiError && e.api.status === 400
          ? "invalid"
          : "network";
    } finally {
      tzBusy = false;
    }
  }
</script>

<svelte:head><title>{settings_title()}</title></svelte:head>

<div class="h-full overflow-y-auto">
  <div class="px-2 py-4 md:p-6 max-w-md space-y-6">
    <h1 class="text-xl font-semibold hidden md:block">
      {settings_heading()}
    </h1>

    <section class="space-y-3">
      <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
        {settings_appearance()}
      </h2>
      <div
        class="card preset-tonal-surface p-4 flex items-center justify-between gap-3"
      >
        <div class="text-sm">{settings_dark_mode()}</div>
        <LightSwitch />
      </div>
      <div
        class="card preset-tonal-surface p-4 flex items-center justify-between gap-3"
      >
        <div class="text-sm">{settings_language()}</div>
        <LanguageSwitch />
      </div>
    </section>

    {#if mode === "offline"}
      <section class="space-y-3">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
          {settings_timezone()}
        </h2>
        <div class="card preset-tonal-surface space-y-2 p-4">
          <p class="text-sm opacity-70">{settings_timezone_desc()}</p>
          <input
            class="input text-sm"
            type="text"
            list="timezone-candidates"
            bind:value={tzInput}
            placeholder={settings_timezone_placeholder()}
            aria-label={settings_timezone()}
          />
          <datalist id="timezone-candidates">
            {#each tzCandidates as zone (zone)}
              <option value={zone}></option>
            {/each}
          </datalist>
          <div class="flex items-center gap-2">
            <button
              class="btn preset-filled-primary-500 text-sm"
              disabled={tzBusy || !timezoneLoad.ok}
              onclick={onTimezoneSave}
            >
              {common_save()}
            </button>
            <button
              class="btn preset-outlined-surface-500 hover:preset-filled-surface-500 text-sm"
              disabled={tzBusy || !timezoneLoad.ok}
              onclick={() => {
                tzInput = "";
                void onTimezoneSave();
              }}
            >
              {settings_timezone_clear()}
            </button>
          </div>
          {#if tzFeedback === "saved"}
            <p class="text-sm preset-text-success-500">
              {settings_timezone_saved()}
            </p>
          {:else if tzFeedback === "invalid"}
            <p class="text-sm preset-text-error-500">
              {settings_timezone_invalid()}
            </p>
          {:else if tzFeedback === "network"}
            <p class="text-sm preset-text-error-500">
              {error_unknown()}
            </p>
          {/if}
        </div>
      </section>
    {/if}

    <section class="space-y-3">
      <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
        {chat_notifications_label()}
      </h2>
      <div class="card preset-tonal-surface space-y-2 p-4">
        <div class="flex items-center justify-between gap-3">
          <p class="text-sm">{chat_notifications_desc()}</p>
          <Switch
            checked={notificationsEnabled}
            onCheckedChange={onNotificationsToggle}
          >
            <Switch.Control>
              <Switch.Thumb />
            </Switch.Control>
            <Switch.HiddenInput aria-label={chat_notifications_label()} />
          </Switch>
        </div>
        {#if notifyPermission === "denied"}
          <p class="text-error-500 dark:text-error-400 text-xs">
            {chat_notifications_denied()}
          </p>
        {/if}
      </div>
    </section>

    {#if mode === "offline"}
      <section class="space-y-3">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
          {settings_connection()}
        </h2>
        <div class="card preset-tonal-surface p-4 space-y-3">
          <div class="flex items-center gap-2 text-sm">
            <span
              class="size-2 rounded-full {channelsStore.localConnected
                ? 'bg-success-500'
                : 'bg-error-500'}"
              aria-hidden="true"
            ></span>
            <span class="font-medium"
              >{channelsStore.localConnected
                ? settings_connected()
                : settings_disconnected()}</span
            >
          </div>
          <div class="text-xs opacity-60 font-mono break-all">{offlineUrl}</div>
          <div class="flex flex-wrap gap-2">
            {#if channelsStore.localConnected}
              <button
                class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
                onclick={disconnect}>{settings_disconnect()}</button
              >
            {:else}
              <a href="/connect" class="btn btn-sm preset-filled-primary-500"
                >{settings_reconnect()}</a
              >
            {/if}
          </div>
        </div>
      </section>
    {/if}
  </div>
</div>
