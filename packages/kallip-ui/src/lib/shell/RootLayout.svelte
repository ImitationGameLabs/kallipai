<script lang="ts">
  import { onMount } from "svelte";
  import type { Snippet } from "svelte";
  import AppShell from "../../components/AppShell.svelte";
  import AccountMenu from "../../components/AccountMenu.svelte";
  import { classifyError } from "../errors.ts";
  import { agoraSession } from "../session/agora.svelte";
  import { sessionStore } from "../session/session.svelte";
  import { connectDirect } from "../session/connect.ts";
  import { configStore } from "../config/config.svelte";
  import { modeOf } from "../config/mode.ts";
  import { navFor, type NavIcons } from "./links.ts";
  import { appGateDecision, isPublicRoute } from "./gate.ts";
  import { navigate } from "./port.ts";

  let {
    pathname,
    search,
    icons,
    children,
  }: {
    pathname: string;
    search: string;
    icons: NavIcons;
    children: Snippet;
  } = $props();

  // The mode is the single source of "which product are we in", read from the
  // persisted config's `activeMode` (null config defaults to online).
  const mode = $derived(modeOf(configStore.value));

  // Boot once the config has loaded. The two modes need different boot:
  //   - offline: reconnect the daemon straight away (offline's whole point is
  //     the daemon; on failure surface the error and the connect page will
  //     prompt);
  //   - online: resolve the agora session so the gate reads a settled `user`.
  // onMount (not a reactive $effect) so this runs exactly once, with no
  // `booted` flag and no effect read-of-write hazard.
  onMount(() => {
    void configStore.ready.then(() => {
      const cfg = configStore.value;
      if (cfg?.activeMode === "offline" && cfg.offline) {
        // Surface a boot-reconnect failure on the banner (the same classifier
        // the layout uses for mid-session errors) instead of swallowing it --
        // attach() is never reached on failure, so its error reset does not
        // apply; setting `error` directly is correct here.
        connectDirect(cfg.offline)
          .then((s) => sessionStore.attach(s))
          .catch((e) => {
            sessionStore.error = e;
          });
      } else {
        void agoraSession.whoami();
      }
    });
  });

  const decision = $derived(
    appGateDecision({
      loaded: configStore.loaded,
      mode,
      user: agoraSession.user,
      authError: agoraSession.authError,
      connected: sessionStore.connected,
      pathname,
      search,
    }),
  );

  // Act on a redirect decision. replaceState so the guarded URL never enters
  // history (Back returns to the pre-app referrer, not a redirect loop).
  $effect(() => {
    if (decision.kind === "redirect") {
      void navigate(decision.url, { replaceState: true });
    }
  });

  const links = $derived(navFor({ mode, icons }));

  function isActive(href: string): boolean {
    return href === "/" ? pathname === "/" : pathname.startsWith(href);
  }

  // The banner shows a classified, human-readable message; the full error (with
  // cause chain) is mirrored to the console for diagnostics. Offline mode only
  // contacts the daemon (boot reconnect + explicit actions), so this fires on a
  // mid-session daemon failure -- never on an online landing.
  const errorView = $derived(
    sessionStore.error ? classifyError(sessionStore.error) : null,
  );
  $effect(() => {
    if (sessionStore.error) console.error(sessionStore.error);
  });
</script>

<!-- Sidebar footer entry; see AccountMenu for behavior. -->
{#snippet statusSnippet()}
  <AccountMenu />
{/snippet}

{#if decision.kind === "render" && isPublicRoute(pathname)}
  {@render children()}
{:else if decision.kind === "render"}
  <AppShell {links} {isActive} error={errorView} status={statusSnippet}>
    {@render children()}
  </AppShell>
{:else}
  <!-- skeleton: config still loading (mode unknown) or whoami in flight (online,
       no error yet). An auth failure routes the user to /login (see
       appGateDecision), so this branch is only the brief resolving window.
       Never a protected AppShell, so no gated content flashes. -->
  <div class="p-4"><p class="opacity-60">Loading…</p></div>
{/if}
