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

  const notice = $derived(error ?? agoraSession.authError);
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
            const fromUrl = new URL(text, window.location.origin).searchParams
              .get("code");
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
        return "Passkey prompt cancelled.";
      case "invalid-code":
        return "That pairing code is invalid, expired, or already used.";
      case "duplicate-credential":
        return "That device is already registered.";
      case "rate-limited":
        return "Too many attempts. Wait a moment and try again.";
      default:
        return r.message ?? "Pairing failed.";
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

<svelte:head><title>KallipAI · add device</title></svelte:head>

{#if notice}
  <Banner floating title={`Couldn't reach the server: ${notice}`} />
{/if}

<div class="flex items-center justify-center min-h-dvh p-4 bg-surface-100">
  <form
    class="w-full max-w-sm space-y-6 p-6 bg-surface-50 border border-surface-200 shadow-sm rounded-xl"
    onsubmit={submit}
  >
    <div class="text-center space-y-1">
      <Brand size="lg" />
      <p class="text-sm opacity-60">Add this device to your account</p>
    </div>

    <label class="block space-y-1">
      <span class="text-sm opacity-70">
        Pairing code <span class="text-error-500">*</span>
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
      <span class="text-sm opacity-70">Device name</span>
      <input
        class="input"
        autocomplete="off"
        placeholder="e.g. iPhone"
        maxlength={64}
        bind:value={label}
      />
    </label>

    {#if scanning}
      <div class="space-y-2">
        <!-- The camera feed the scanner decodes from. -->
        <video bind:this={videoEl} class="w-full rounded" autoplay muted playsinline></video>
        <button
          type="button"
          class="btn btn-sm preset-tonal-surface"
          onclick={stopScan}>Cancel scan</button
        >
        {#if scanError}
          <div class="text-xs text-error-600">Camera unavailable: {scanError}</div>
        {/if}
      </div>
    {:else}
      <button
        type="button"
        class="text-xs text-primary-500 hover:underline"
        onclick={startScan}>Scan QR instead</button
      >
    {/if}

    {#if result && !result.ok}
      <p role="alert" class="text-sm text-error-500">
        {reasonMessage(result)}
      </p>
    {/if}

    <button
      type="submit"
      class="btn preset-filled-primary-500 w-full"
      disabled={!canSubmit}
    >
      {submitting ? "Adding…" : "Add this device"}
    </button>

    <p class="text-center text-sm">
      Already signed in here?
      <a
        href="/login"
        class="font-medium text-primary-500 hover:underline cursor-pointer"
        >Sign in</a
      >
    </p>
  </form>
</div>
