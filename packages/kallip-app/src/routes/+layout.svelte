<script lang="ts">
  import { goto } from "$app/navigation";
  import { page } from "$app/state";
  import "../app.css";
  import {
    RootLayout,
    initShell,
    initAgora,
    initConfigStorage,
    initLesche,
    localStorageConfigStorage,
    type NavIcons,
  } from "@kallipai/kallip-ui";
  import { Calendar, Cpu, FolderCog, LayoutGrid, MessageSquare, Settings, Users, Wallet } from "@lucide/svelte";

  // Inject the app's navigation, agora/lesche URLs, and storage backend into
  // kallip-ui. The shared <RootLayout> consumes these ports (it cannot import
  // $app/* or import.meta.env from inside the library package). Idempotent
  // setters; the root layout has a single instance so this runs once at boot.
  // NOTE: Tauri swaps localStorageConfigStorage for a secure-storage adapter
  // once the plugin is wired. The WebAuthn passkey ceremony in this webview is
  // gated on Tauri webview origin support.
  initShell(goto);
  initAgora(import.meta.env.VITE_AGORA_URL ?? "http://localhost:7100");
  initLesche(import.meta.env.VITE_LESCHE_URL ?? "http://localhost:7200");
  initConfigStorage(localStorageConfigStorage);

  const icons: NavIcons = {
    chat: MessageSquare,
    tagmata: Cpu,
    rooms: Users,
    settings: Settings,
    manageOverview: LayoutGrid,
    manageHub: FolderCog,
    manageBudget: Wallet,
    manageAgents: Users,
    manageProfiles: Settings,
    manageSchedules: Calendar,
  };

  let { children } = $props();
</script>

<RootLayout pathname={page.url.pathname} search={page.url.search} {icons}>
  {@render children()}
</RootLayout>
