<script lang="ts">
  import { goto } from "$app/navigation";
  import { page } from "$app/state";
  import "../app.css";
  import {
    RootLayout,
    initShell,
    initAgora,
    initConfigStorage,
    localStorageConfigStorage,
    type NavIcons,
  } from "@kallipai/kallip-ui";
  import { ClipboardCheck, Cpu, MessageSquare } from "@lucide/svelte";

  // Inject the app's navigation, agora URL, and storage backend into kallip-ui.
  // The shared <RootLayout> consumes these ports (it cannot import $app/* or
  // import.meta.env from inside the library package). Idempotent setters; the
  // root layout has a single instance so this runs once at boot.
  // NOTE (Phase 4): Tauri swaps localStorageConfigStorage for a secure-storage
  // adapter once the plugin is wired. The WebAuthn passkey ceremony in this
  // webview is gated on the Phase 4 origin spike.
  initShell(goto);
  initAgora(import.meta.env.VITE_AGORA_URL ?? "http://localhost:7100");
  initConfigStorage(localStorageConfigStorage);

  const icons: NavIcons = {
    chat: MessageSquare,
    approvals: ClipboardCheck,
    tagmata: Cpu,
  };

  let { children } = $props();
</script>

<RootLayout pathname={page.url.pathname} search={page.url.search} {icons}>
  {@render children()}
</RootLayout>
