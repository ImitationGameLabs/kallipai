<script lang="ts">
  import { goto } from "$app/navigation";
  import { page } from "$app/state";
  import "../app.css";
  import {
    RootLayout,
    initShell,
    setOfflineOnlyShell,
    initConfigStorage,
    localStorageConfigStorage,
    type NavIcons,
  } from "@kallipai/kallip-ui";
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

  // Inject the app's navigation and storage backend into kallip-ui. The shared
  // <RootLayout> consumes these ports (it cannot import $app/* from inside
  // the library package). Idempotent setters; the root layout has a single
  // instance so this runs once at boot.
  // No initArcheion/initLesche/initInstances: this shell is offline-only. The
  // RootLayout offline boot (configStore-driven connectDirect) never reads
  // the relay ports, and the instances capability probe tolerates an unset
  // service (its UI hides when unreachable).
  // Declare the offline-only identity first: kallip-ui derives the product
  // mode (gate, nav, account chrome) and the boot shape from it.
  setOfflineOnlyShell();
  initShell(goto);
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
