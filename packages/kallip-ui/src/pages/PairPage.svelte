<script lang="ts">
  // Pair THIS (anonymous) device onto an existing account: enter (or scan) a
  // short code minted by an already-signed-in device, then create a LOCAL
  // passkey. On success this device is signed in. Mirrors RegisterPage's shape.
  import { onMount } from "svelte";
  import { agoraSession } from "../lib/session/agora.svelte";
  import { navigate } from "../lib/shell/port.ts";
  import type { PairResult } from "@kallipai/kallip-agora-client";
  import Brand from "../components/Brand.svelte";
  import Banner from "../components/Banner.svelte";
  import FormError from "../components/FormError.svelte";
  import {
    auth_passkey_cancelled,
    auth_rate_limited,
    auth_couldnt_reach,
    auth_add_this_device,
    auth_sign_in,
    settings_passkey_duplicate,
    pair_title,
    pair_subtitle,
    pair_code_label,
    pair_device_name,
    pair_device_placeholder,
    pair_cancel_scan,
    pair_camera_unavailable,
    pair_scan_qr,
    pair_invalid_code,
    pair_failed,
    pair_adding,
    pair_already,
  } from "../paraglide/messages.js";

  // A pairing-code link can pre-fill via ?code=... (e.g. the QR deep link).
  let { code: initialCode = "" }: { code?: string } = $props();
  let code = $state("");
  let label = $state("");
  let submitting = $state(false);
  let result: PairResult | null = $state(null);
  let error = $state<string | null>(null);

  // Optional camera QR scan. The scanner lib is heavy, so it is dynamically
  // imported only when the user toggles it on.
  let scanning = $state(false);
  let scanError = $state<string | null>(null);
  let videoEl: HTMLVideoElement | null = $state(null);
  let controls: { stop: () => void } | null = null;

  const canSubmit = $derived(code.trim().length > 0 && !submitting);

  onMount(() => {
    if (initialCode) code = initialCode;
  });

  async function startScan() {
    scanning = true;
    scanError = null;
    try {
      const { BrowserMultiFormatReader } = await import("@zxing/browser");
      const reader = new BrowserMultiFormatReader();
      // decodeFromVideoDevice resolves once the camera + decoder are running;
      // the callback fires on each decoded frame.
      controls = await reader.decodeFromVideoDevice(
        undefined,
        videoEl!,
        (decoded, _err) => {
          if (decoded) {
            // Accept either the raw code or a `${origin}/pair?code=...` deep link.
            const text = decoded.getText();
            const fromUrl = new URL(
              text,
              window.location.origin,
            ).searchParams.get("code");
            code = fromUrl ?? text;
            stopScan();
          }
        },
      );
    } catch (e) {
      scanError = e instanceof Error ? e.message : String(e);
      scanning = false;
    }
  }

  function stopScan() {
    controls?.stop();
    controls = null;
    scanning = false;
  }

  function reasonMessage(r: PairResult): string | null {
    if (r.ok) return null;
    switch (r.reason) {
      case "cancelled":
        return auth_passkey_cancelled();
      case "invalid-code":
        return pair_invalid_code();
      case "duplicate-credential":
        return settings_passkey_duplicate();
      case "rate-limited":
        return auth_rate_limited();
      default:
        return r.message ?? pair_failed();
    }
  }

  async function submit(e: Event) {
    e.preventDefault();
    if (!canSubmit) return;
    submitting = true;
    result = null;
    error = null;
    try {
      const r = await agoraSession.pairDevice(code.trim(), label.trim());
      result = r;
      if (r.ok) await navigate("/tagmata");
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }
</script>

<svelte:head><title>{pair_title()}</title></svelte:head>
{#if agoraSession.authError}
  <!-- Environment error (agora unreachable at boot); a submit's own failures
       render inline in the form below. -->
  <Banner floating title={auth_couldnt_reach({ notice: agoraSession.authError })} />
{/if}

<div class="flex items-center justify-center min-h-dvh p-4 bg-surface-100-900">
  <form
    class="w-full max-w-sm space-y-6 p-6 bg-surface-50-950 border border-surface-200-800 shadow-sm rounded-xl"
    onsubmit={submit}
  >
    <div class="text-center space-y-1">
      <Brand size="lg" />
      <p class="text-sm opacity-60">{pair_subtitle()}</p>
    </div>

    <label class="block space-y-1">
      <span class="text-sm opacity-70">
        {pair_code_label()}
        <span class="text-error-500 dark:text-error-400">*</span>
      </span>
      <input
        class="input font-mono tracking-widest"
        autocomplete="off"
        placeholder="XXXX-XXXX"
        bind:value={code}
        required
      />
    </label>

    <label class="block space-y-1">
      <span class="text-sm opacity-70">{pair_device_name()}</span>
      <input
        class="input"
        autocomplete="off"
        placeholder={pair_device_placeholder()}
        maxlength={64}
        bind:value={label}
      />
    </label>

    {#if scanning}
      <div class="space-y-2">
        <!-- The camera feed the scanner decodes from. -->
        <video
          bind:this={videoEl}
          class="w-full rounded"
          autoplay
          muted
          playsinline
        ></video>
        <button
          type="button"
          class="btn btn-sm preset-tonal-surface"
          onclick={stopScan}>{pair_cancel_scan()}</button
        >
        {#if scanError}
          <div class="text-xs text-error-600 dark:text-error-500">
            {pair_camera_unavailable({ error: scanError })}
          </div>
        {/if}
      </div>
    {:else}
      <button
        type="button"
        class="text-xs text-primary-500 dark:text-primary-400 hover:underline"
        onclick={startScan}>{pair_scan_qr()}</button
      >
    {/if}
    {#if error}
      <FormError message={auth_couldnt_reach({ notice: error })} />
    {:else if result && !result.ok}
      <FormError message={reasonMessage(result)} />
    {/if}

    <button
      type="submit"
      class="btn preset-filled-primary-500 w-full"
      disabled={!canSubmit}
    >
      {submitting ? pair_adding() : auth_add_this_device()}
    </button>

    <p class="text-center text-sm">
      {pair_already()}
      <a
        href="/login"
        class="font-medium text-primary-500 dark:text-primary-400 hover:underline cursor-pointer"
        >{auth_sign_in()}</a
      >
    </p>
  </form>
</div>
