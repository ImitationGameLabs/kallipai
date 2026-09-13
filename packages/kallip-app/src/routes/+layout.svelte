<script lang="ts">
  import { goto } from "$app/navigation";
  import { page } from "$app/state";
  import "../app.css";
  import {
    RootLayout,
    initShell,
    initArcheion,
    initConfigStorage,
    initFiles,
    initNotificationBackend,
    initInstances,
    initLesche,
    localStorageConfigStorage,
    type NavIcons,
  } from "@kallipai/kallip-ui";
  import { tauriNotificationBackend } from "../lib/tauri-notification.ts";
  import {
    Calendar,
    Cpu,
    Folder,
    House,
    LayoutGrid,
    MessageSquare,
    Settings,
    Users,
    Wallet,
  } from "@lucide/svelte";

  // Inject the app's navigation, archeion/lesche URLs, and storage backend into
  // kallip-ui. The shared <RootLayout> consumes these ports (it cannot import
  // $app/* or import.meta.env from inside the library package). Idempotent
  // setters; the root layout has a single instance so this runs once at boot.
  // NOTE: Tauri swaps localStorageConfigStorage for a secure-storage adapter
  // once the plugin is wired. The WebAuthn passkey ceremony in this webview is
  // gated on Tauri webview origin support.
  initShell(goto);
  // Desktop-surface boundary: this app runs in the Tauri webview on the
  // user's own machine, so same-origin rules do not apply: the app dials
  // the platform API directly. KALLIP_POLIS_URL (baked at build time,
  // exposed via vite's envPrefix) names the platform edge origin; the
  // dev-local default is the local edge. This is not a browser
  // fallback: the web app served from a deployment derives its
  // api.<domain> URLs from the page location and never reaches it.
  //
  // Service URLs on this surface: {KALLIP_POLIS_URL}/v1/<service>.
  const polis = import.meta.env.KALLIP_POLIS_URL ?? "http://localhost:8080";
  initArcheion(`${polis}/v1/archeion`);
  initLesche(`${polis}/v1/lesche`);
  initFiles(`${polis}/v1/files`);
  initConfigStorage(localStorageConfigStorage);
  initNotificationBackend(tauriNotificationBackend);
  initInstances(`${polis}/v1/instances`);

  const icons: NavIcons = {
    chat: MessageSquare,
    tagmata: Cpu,
    rooms: Users,
    settings: Settings,
    manageOverview: LayoutGrid,
    home: House,
    manageBudget: Wallet,
    manageAgents: Users,
    manageProfiles: Settings,
    manageSchedules: Calendar,
    files: Folder,
  };

  let { children } = $props();
</script>

<RootLayout
  pathname={page.url.pathname}
  search={page.url.search}
  appKind="app"
  {icons}
>
  {@render children()}
</RootLayout>
