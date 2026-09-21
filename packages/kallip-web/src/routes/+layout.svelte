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
    initInstances,
    initLesche,
    localStorageConfigStorage,
    type NavIcons,
  } from "@kallipai/kallip-ui";
  import { serviceUrl } from "../lib/service-urls.ts";
  import {
    Calendar,
    Cpu,
    Folder,
    House,
    LayoutGrid,
    ListTodo,
    MessageSquare,
    Settings,
    Users,
    Wallet,
  } from "@lucide/svelte";

  // Inject the app's navigation, archeion/lesche URLs, and storage backend into
  // kallip-ui. The shared <RootLayout> consumes these ports (it cannot import
  // $app/* or import.meta.env from inside the library package). Idempotent
  // setters; the root layout has a single instance so this runs once at boot.
  initShell(goto);
  // Service URLs resolve at runtime in two layers: the deployment
  // config from /config.js (window.KALLIP_CONFIG — the factory file by
  // default, or baked from runtimeConfig by the NixOS module) overrides,
  // otherwise the URL derives from the browser location: the origin the
  // page is on names the deployment domain (web.<domain> strips to
  // <domain>), and the sibling subdomains follow the page's own
  // protocol and port: a page on a non-default port (the dev edge on
  // :8080) reaches its siblings on that port too.
  const config = window.KALLIP_CONFIG ?? {};
  initArcheion(serviceUrl("archeion", config, location));
  initLesche(serviceUrl("lesche", config, location));
  initFiles(serviceUrl("files", config, location));
  initInstances(serviceUrl("instances", config, location));
  initConfigStorage(localStorageConfigStorage);

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
    manageTasks: ListTodo,
    files: Folder,
  };

  let { children } = $props();
</script>

<RootLayout
  pathname={page.url.pathname}
  search={page.url.search}
  appKind="web"
  {icons}
>
  {@render children()}
</RootLayout>
