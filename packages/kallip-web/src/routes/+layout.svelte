<script lang="ts">
  import { onMount } from "svelte";
  import { page } from "$app/state";
  import "../app.css";
  import { sessionStore } from "$lib/session/session.svelte";
  import { loadConfig } from "$lib/config/credentials";
  import { connectDirect } from "$lib/session/connect";
  import {
    AppShell,
    classifyError,
    connectionViewModel,
    type NavItem,
  } from "@kallipai/kallip-ui";
  import { ClipboardCheck, MessageSquare, Settings } from "@lucide/svelte";

  let { children } = $props();

  const links: NavItem[] = [
    { href: "/", label: "Chat", icon: MessageSquare },
    { href: "/approvals", label: "Approvals", icon: ClipboardCheck },
    { href: "/settings", label: "Settings", icon: Settings },
  ];

  function isActive(href: string): boolean {
    return href === "/"
      ? page.url.pathname === "/"
      : page.url.pathname.startsWith(href);
  }

  // The banner shows a classified, human-readable message; the full error (with
  // cause chain) is mirrored to the console for diagnostics.
  const errorView = $derived(
    sessionStore.error ? classifyError(sessionStore.error) : null,
  );
  $effect(() => {
    if (sessionStore.error) console.error(sessionStore.error);
  });

  const connection = $derived(connectionViewModel(sessionStore));

  onMount(() => {
    const config = loadConfig();
    if (config?.backend !== "direct") return;
    sessionStore.connecting = true;
    connectDirect(config)
      .then((session) => sessionStore.attach(session))
      .catch((e: unknown) => {
        sessionStore.error = e;
      })
      .finally(() => {
        sessionStore.connecting = false;
      });
  });
</script>

<AppShell {links} {isActive} error={errorView}>
  {#snippet status()}
    <span class="flex items-center gap-1.5 preset-tonal px-2 py-1 rounded-full">
      <span class="size-2 rounded-full {connection.dotClass}" aria-hidden="true"
      ></span>
      <span class="opacity-70">{connection.label}</span>
    </span>
  {/snippet}
  {@render children()}
</AppShell>
