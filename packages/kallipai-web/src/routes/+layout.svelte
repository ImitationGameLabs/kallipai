<script lang="ts">
  import { goto } from "$app/navigation";
  import { version } from "$app/environment";
  import { base } from "$app/paths";
  import { onMount } from "svelte";
  import { page } from "$app/state";
  import "../app.css";
  import {
    RootLayout,
    initShell,
    initShellIdentity,
    initArcheion,
    initConfigStorage,
    initFiles,
    initInstances,
    initRelays,
    localStorageConfigStorage,
    startVersionWatch,
    type NavIcons,
  } from "@kallipai/kallipai-ui";
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

  // Inject the app's navigation, archeion/lesche/keryx URLs, and storage backend into
  // kallipai-ui. The shared <RootLayout> consumes these ports (it cannot import
  // $app/* or import.meta.env from inside the library package). Idempotent
  // setters; the root layout has a single instance so this runs once at boot.
  initShell(goto);
  // Service URLs resolve at runtime in two layers: the deployment
  // config from /config.js (window.KALLIPAI_CONFIG — the factory file by
  // default, or baked from runtimeConfig by the NixOS module) overrides,
  // otherwise the URL derives from the browser location: the origin the
  // page is on names the deployment domain (web.<domain> strips to
  // <domain>), and the sibling subdomains follow the page's own
  // protocol and port: a page on a non-default port (the dev edge on
  // :8080) reaches its siblings on that port too.
  const config = window.KALLIPAI_CONFIG ?? {};
  // The offline-login flag rides the same runtime config: the factory file
  // ships it as true (the self-hosted posture) and a cloud-facing deployment
  // sets it false, so the shared login page shows the operator-key branch
  // only where the deployment allows it.
  initShellIdentity({ offlineLogin: config.offlineLogin ?? true });
  initArcheion(serviceUrl("archeion", config, location));
  initRelays(
    serviceUrl("lesche", config, location),
    serviceUrl("keryx", config, location),
  );
  initFiles(serviceUrl("files", config, location));
  // The instances declaration rides the same file: a deployment that
  // declares no instance host is not asked for the service (an unset key
  // means it has one).
  initInstances(serviceUrl("instances", config, location), {
    instancesServiceEnabled: config.instancesServiceEnabled,
  });
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

  // A newer bundle may be deployed while this page stays open: the shell
  // asks `_app/version.json` and shows a notice. `version` is this bundle's
  // id, written by the build (see vite.config.ts); the probe carries `?t=`
  // so an edge that caches the file cannot hide the deployment, and the
  // notice never reloads on its own -- the user may be mid-message.
  let updateAvailable = $state(false);

  async function fetchBuildVersion(): Promise<unknown> {
    const res = await fetch(`${base}/_app/version.json?t=${Date.now()}`);
    if (!res.ok) return null;
    const body: unknown = await res.json();
    return typeof body === "object" && body !== null && "version" in body
      ? body.version
      : null;
  }

  onMount(() =>
    startVersionWatch({
      current: version,
      fetchVersion: fetchBuildVersion,
      onAvailable: () => (updateAvailable = true),
    }),
  );

  let { children } = $props();
</script>

<RootLayout
  pathname={page.url.pathname}
  search={page.url.search}
  appKind="web"
  {icons}
  {updateAvailable}
>
  {@render children()}
</RootLayout>
