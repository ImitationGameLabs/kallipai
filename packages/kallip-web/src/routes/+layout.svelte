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
  import {
    Calendar,
    Cpu,
    FolderCog,
    LayoutGrid,
    MessageSquare,
    Settings,
    Users,
    Wallet,
  } from "@lucide/svelte";

  // Inject the app's navigation, agora/lesche URLs, and storage backend into
  // kallip-ui. The shared <RootLayout> consumes these ports (it cannot import
  // $app/* or import.meta.env from inside the library package). Idempotent
  // setters; the root layout has a single instance so this runs once at boot.
  initShell(goto);
  // The dev stack is fronted by Caddy, so the browser reaches the agora/lesche
  // at their *.<devDomain> subdomains. devDomain is injected by vite.config.ts
  // from the same KALLIP_DEV_DOMAIN env var the backend stack uses; explicit
  // VITE_AGORA_URL / VITE_LESCHE_URL still win (e.g. for a prod build or a
  // non-default topology).
  const devDomain = import.meta.env.KALLIP_DEV_DOMAIN ?? "kallipai.com";
  initAgora(import.meta.env.VITE_AGORA_URL ?? `https://agora.${devDomain}`);
  initLesche(import.meta.env.VITE_LESCHE_URL ?? `https://lesche.${devDomain}`);
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
