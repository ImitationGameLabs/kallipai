<script lang="ts">
  // The combined manage page: the Tagmata section is one card per tagma
  // across its lifecycle -- a pending enrollment code, an enrolled identity,
  // and (when one backs the card) its host-side process, joined by the slug
  // prefix convention. The registry half (codes + identities) is archeion-side;
  // the process half is the local process host, so the page is mode-neutral:
  // offline the registry is simply absent and the page degrades to the
  // process list. Online, a second section surfaces Rooms management (rows
  // + the standing /rooms entry); its visibility keys on the shell mode
  // only, never on the rooms fetch state. The page owns every store call;
  // the cards and dialogs stay presentational (the CreateRoomDialog
  // discipline). The AppShell expects the page root to scroll itself
  // (h-full overflow-y-auto).
  import {
    archeionPolisOriginOrFail,
    archeionClientOrFail,
    archeionSession,
    lescheBaseUrlOrFail,
    lescheClientOrFail,
  } from "../lib/session/archeion.svelte";
  import { channelsStore } from "../lib/session/channels.svelte";
  import { roomsStore } from "../lib/session/rooms.svelte";
  import { openRelayChannel } from "@kallipai/kallip-lesche-client";
  import { type ProviderSummary } from "@kallipai/kallip-archeion-client";
  import { OnlineBackend } from "../lib/manage/backend.ts";
  import {
    ManageRestClient,
    ProjectionClient,
  } from "@kallipai/kallip-lesche-client";
  import { shellMode } from "../lib/shell/port.ts";
  import {
    isLocked,
    providerEndpointKey,
    pushCredentials,
    type PushOutcome,
    type PushPorts,
  } from "../lib/instances/credentialPush.ts";
  import { realtimeStore } from "../lib/session/realtime.svelte.ts";
  import { instancesStore } from "../lib/instances/instances.svelte.ts";
  import {
    CONNECT_TOKEN_KEY,
    INSTANCES_TOKEN_KEY,
    InstancesError,
    instanceSlugFor,
  } from "../lib/instances/client.ts";
  import { joinDeviceRows } from "../lib/tagmata.svelte.ts";
  import ConfirmDialog from "../components/ConfirmDialog.svelte";
  import CreateInstanceDialog, {
    type AdvancedSpawnFields,
  } from "../components/instances/CreateInstanceDialog.svelte";
  import EnrollmentCodeCard from "../components/tagmata/EnrollmentCodeCard.svelte";
  import TagmaCard from "../components/tagmata/TagmaCard.svelte";
  import HubRow from "../components/HubRow.svelte";
  import { Users } from "@lucide/svelte";
  import {
    manage_instances_create_failed,
    manage_instances_create_mint_failed,
    manage_instances_create_provider_locked_hint,
    manage_instances_empty,
    manage_instances_error_bad_request,
    manage_instances_error_internal,
    manage_instances_error_invalid_spawn_input,
    manage_instances_error_not_found,
    manage_instances_error_not_running,
    manage_instances_error_slug_taken,
    manage_instances_error_spawn_timeout,
    manage_instances_error_workspace_overlap,
    manage_instances_forbidden,
    manage_instances_host_forbidden,
    manage_instances_load_failed,
    manage_instances_loading,
    manage_instances_new,
    manage_instances_push_failed,
    manage_instances_push_pending,
    manage_instances_push_not_applied,
    manage_instances_push_unreachable,
    manage_instances_push_unverified,
    manage_instances_push_verified,
    manage_instances_spawn_success,
    manage_instances_start,
    manage_instances_stop,
    manage_instances_stop_description,
    manage_instances_stop_title,
    manage_instances_token_apply,
    manage_instances_token_label,
    manage_instances_token_placeholder,
    manage_instances_token_rejected,
    manage_instances_unauthorized,
    manage_instances_session_required,
    manage_instances_unreachable,
    common_loading,
    nav_manage,
    nav_rooms,
    nav_chat,
    nav_tagmata,
    tagma_rooms_section_empty,
    tagma_rooms_section_manage,
    room_label_fallback,
    tagmata_load_failed,
    rooms_retrying,
    tagmata_new,
    tagmata_new_hint,
    tagmata_title,
  } from "../paraglide/messages.js";

  $effect(() => {
    instancesStore.startPolling(5000);
    return () => instancesStore.stopPolling();
  });

  // --- create dialog (the two-path "New tagma" flow) ---------------------
  let createOpen = $state(false);
  let createBusy = $state(false);
  let createError = $state<string | null>(null);
  // The just-spawned success line lives here (not in the dialog): it must
  // outlive the dialog, which closes on success. The credential push runs
  // after spawn returns, so its own status rides alongside.
  let spawnResult = $state<{ slug: string; port: number } | null>(null);
  let pushStatus = $state<{
    slug: string;
    kind: "pending" | PushOutcome["state"] | "locked" | "not-applied";
    probeOk?: boolean;
    message?: string;
  } | null>(null);

  const canSpawn = $derived(
    instancesStore.capabilities?.includes("designated-user") ?? false,
  );

  // Service error codes to their localized line; the spawn paths and the
  // stop dialog share this mapping through faultLine.
  const codeMessage: Record<string, () => string> = {
    slug_taken: manage_instances_error_slug_taken,
    workspace_overlap: manage_instances_error_workspace_overlap,
    invalid_spawn_input: manage_instances_error_invalid_spawn_input,
    spawn_timeout: manage_instances_error_spawn_timeout,
    not_found: manage_instances_error_not_found,
    not_running: manage_instances_error_not_running,
    bad_request: manage_instances_error_bad_request,
    internal: manage_instances_error_internal,
  };

  function faultLine(cause: unknown): string {
    if (cause instanceof InstancesError) {
      const line = cause.code ? codeMessage[cause.code] : undefined;
      if (line) {
        return line();
      }
      if (cause.kind === "unauthorized") {
        return manage_instances_unauthorized();
      }
    }
    return manage_instances_error_internal();
  }

  // One-click (dialog path A): mint, then spawn with the relay env
  // pointing at this deployment so the tagma enrolls itself on first
  // start. On success close the dialog and surface the port line.
  async function onOneClick(opts: {
    workspace: string;
    providerId?: string | null;
    model?: string;
  }): Promise<void> {
    createBusy = true;
    spawnResult = null;
    pushStatus = null;
    createError = null;
    try {
      const minted = await archeionSession.mintTagma();
      if (!minted) {
        createError = manage_instances_create_mint_failed();
        return;
      }
      const slug = instanceSlugFor(minted.id);
      const env = [
        `KALLIP_POLIS_URL=${archeionPolisOriginOrFail()}`,
        `KALLIP_TAGMA_RELAY_ENROLLMENT_CODE=${minted.code}`,
      ];
      try {
        const result = await instancesStore.spawn({
          slug,
          workspace: opts.workspace,
          env,
        });
        spawnResult = { slug: result.slug, port: result.port };
        createOpen = false;
        if (opts.providerId) {
          void kickCredentialPush(
            minted.id,
            result.slug,
            opts.providerId,
            opts.model ?? "",
          );
        }
      } catch (cause) {
        // The minted code stays valid (the pending card shows its masked
        // form); the advanced path can redeem it by hand.
        console.error("[tagmata] one-click spawn failed:", cause);
        createError = manage_instances_create_failed();
      }
    } finally {
      createBusy = false;
    }
  }

  /** Resolve the vault row the dialog picked into usable key material and
   *  hand it to the push loop over a dedicated relay channel (closed when
   *  the outcome lands). Belt-and-braces on the dialog's fail-closed gate:
   *  a locked or unresolvable entry surfaces an error instead of pushing.
   *  Fire-and-forget by design -- spawn already succeeded; credential
   *  delivery reports through pushStatus, never blocks this handler. */
  async function kickCredentialPush(
    tagmaId: string,
    slug: string,
    providerId: string,
    model: string,
  ): Promise<void> {
    pushStatus = { slug, kind: "pending" };
    const entry = archeionSession.providers.find((p) => p.id === providerId);
    const viaPasskey = archeionSession.canFlipKeys();
    if (!entry || isLocked(entry, viaPasskey)) {
      pushStatus = { slug, kind: "locked" };
      return;
    }
    let apiKey: string | null;
    if (entry.mode === "plaintext") {
      apiKey = entry.key_material;
    } else {
      apiKey = await archeionSession.revealProviderKey(entry);
      if (!apiKey) {
        // Sealed on another device: not deliverable from THIS session.
        pushStatus = { slug, kind: "locked" };
        return;
      }
    }
    const target = {
      endpointKey: providerEndpointKey(slug),
      family: entry.provider,
      apiKey,
      baseUrl: entry.base_url,
      model,
    };
    try {
      const user = archeionSession.user;
      if (!user) throw new Error("not signed in");
      const info = await archeionClientOrFail().getTagma(tagmaId);
      const channel = await openRelayChannel(
        lescheClientOrFail(),
        tagmaId,
        user.user_id,
        user.display_name ?? user.username ?? user.user_id,
        info.pinned_public_key,
      );
      const backend = new OnlineBackend(
        // The projection drives the status card's read plane (roster, budget,
        // work schedule, SSE feed); profile saves ride the E2E
        // envelope channel below (keys never ride plaintext frames).
        new ManageRestClient(lescheBaseUrlOrFail()),
        channel.tagmaId,
        new ProjectionClient(lescheBaseUrlOrFail()),
        channel,
      );
      const ports: PushPorts = {
        fetchLive: () => backend.getProfiles(),
        put: (body) => backend.updateProfiles(body),
        probe: (body) => backend.probeProfiles(body),
        apply: () => backend.applyProfiles(),
        now: () => Date.now(),
        sleep: (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
        close: () => channel.close(),
      };
      const outcome = await pushCredentials(target, ports);
      applyPushOutcome(slug, outcome);
    } catch (cause) {
      // Channel/KEX setup never got the loop started: fold to unreachable
      // (the tagma may simply not be enrolled yet).
      console.error("[tagmata] credential push channel failed:", cause);
      pushStatus = { slug, kind: "unreachable" };
    }
  }

  function applyPushOutcome(slug: string, outcome: PushOutcome): void {
    switch (outcome.state) {
      case "pushed":
      case "pushed":
        pushStatus =
          outcome.applied >= 1
            ? { slug, kind: "pushed", probeOk: outcome.probeOk }
            : { slug, kind: "not-applied" };
        break;
      case "unreachable":
        pushStatus = { slug, kind: "unreachable" };
        break;
      case "failed":
        pushStatus = { slug, kind: "failed", message: outcome.message };
        break;
    }
  }

  // Advanced (dialog path A's disclosure): assemble the daemon env
  // allowlist from the fixed optional fields -- no free-form KEY=VALUE
  // entry (the daemon validates keys).
  async function onSpawn(f: AdvancedSpawnFields): Promise<void> {
    createBusy = true;
    spawnResult = null;
    createError = null;
    const env: string[] = [];
    if (f.polisUrl.trim()) {
      env.push("KALLIP_POLIS_URL=" + f.polisUrl.trim());
    }
    if (f.enrollmentCode.trim()) {
      env.push("KALLIP_TAGMA_RELAY_ENROLLMENT_CODE=" + f.enrollmentCode.trim());
    }
    if (f.instanceToken.trim()) {
      env.push("KALLIP_AUTH_TOKEN=" + f.instanceToken.trim());
    }
    if (f.llmProvider.trim()) {
      env.push("KALLIP_LLM_PROVIDER=" + f.llmProvider.trim());
    }
    if (f.llmModel.trim()) {
      env.push("KALLIP_LLM_MODEL=" + f.llmModel.trim());
    }
    // The key variable name follows the provider choice.
    if (f.llmApiKey.trim()) {
      const keyVar =
        f.llmProvider.trim() === "openai-compatible"
          ? "KALLIP_LLM_OPENAI_COMPAT_API_KEY"
          : "KALLIP_LLM_DEEPSEEK_API_KEY";
      env.push(keyVar + "=" + f.llmApiKey.trim());
    }
    try {
      const result = await instancesStore.spawn({
        slug: f.slug,
        workspace: f.workspace,
        env,
      });
      spawnResult = { slug: result.slug, port: result.port };
      if (f.instanceToken.trim()) {
        sessionStorage.setItem(CONNECT_TOKEN_KEY, f.instanceToken.trim());
      }
      createOpen = false;
    } catch (cause) {
      createError = faultLine(cause);
    } finally {
      createBusy = false;
    }
  }

  // Local-agent (dialog path B): mint only; the dialog holds the
  // plaintext + QR. mintTagma self-reports failure (null + its own error
  // state), so map null to the dialog's error line here.
  async function onMint(): Promise<{ id: string; code: string } | null> {
    createBusy = true;
    createError = null;
    try {
      const minted = await archeionSession.mintTagma();
      if (!minted) createError = manage_instances_create_mint_failed();
      return minted;
    } finally {
      createBusy = false;
    }
  }

  // --- stop dialog (the hosted card's Stop action) ------------------------
  let stopTarget = $state<string | null>(null);
  let stopBusy = $state(false);
  let stopError = $state<string | null>(null);

  // --- start action (the hosted card's Start menu item) -------------------
  // One relaunch at a time (a start blocks for seconds); failures surface
  // through the same code mapping as spawn/stop.
  let startBusySlug = $state<string | null>(null);
  let startError = $state<string | null>(null);

  async function onStartClick(slug: string): Promise<void> {
    if (startBusySlug !== null || !slug) return;
    startBusySlug = slug;
    startError = null;
    try {
      await instancesStore.start(slug);
    } catch (cause) {
      startError = faultLine(cause);
    } finally {
      startBusySlug = null;
    }
  }

  async function onStopConfirmed() {
    if (!stopTarget || stopBusy) return;
    stopBusy = true;
    stopError = null;
    try {
      await instancesStore.stop(stopTarget);
      stopTarget = null;
    } catch (cause) {
      stopError = faultLine(cause);
    } finally {
      stopBusy = false;
    }
  }

  // --- standalone-mode token entry (the 401 banner) -----------------------
  let tokenInput = $state("");
  let tokenRejected = $state(false);

  async function onTokenApply(event: SubmitEvent) {
    event.preventDefault();
    if (!tokenInput.trim()) return;
    sessionStorage.setItem(INSTANCES_TOKEN_KEY, tokenInput.trim());
    await instancesStore.refresh();
    tokenRejected = instancesStore.errorKind === "unauthorized";
  }

  const kindMessage = {
    unauthorized: manage_instances_unauthorized,
    forbidden: manage_instances_forbidden,
    unreachable: manage_instances_unreachable,
    other: manage_instances_load_failed,
  } as const;

  // --- the card list -------------------------------------------------------
  // Join identities with hosted processes: the process-reported tagma_id
  // first (the daemon scan reads the tagma's own persisted id), the one-click
  // slug convention as fallback (see joinDeviceRows for the key order).
  const devices = $derived(
    joinDeviceRows(
      archeionSession.enrolledCards,
      instancesStore.instances,
      instancesStore.spawnedPorts,
      instanceSlugFor,
      (id) =>
        realtimeStore.resolved
          ? realtimeStore.has(id)
            ? "online"
            : "offline"
          : "checking",
      (id) => realtimeStore.statusFor(id),
    ),
  );

  const pending = $derived(archeionSession.pending);

  // Both halves must settle before the empty state may fire (the process
  // list loaded or failed; the registry loaded or failed for the signed-in
  // user) -- else a "no tagmas" hero flashes over an in-flight fetch. A
  // daemon error keeps the banner up instead of the hero.
  const settled = $derived(
    (instancesStore.loaded || instancesStore.errorKind !== null) &&
      (archeionSession.user == null ||
        archeionSession.tagmataLoaded ||
        archeionSession.tagmataError !== null),
  );
  const empty = $derived(
    settled &&
      instancesStore.errorKind === null &&
      devices.length === 0 &&
      pending.length === 0,
  );

  function openCreate() {
    createError = null;
    createOpen = true;
  }

  // Revoke tears down the revoked tagma's open channel + purges its cache,
  // so a shared device does not keep the previous user's plaintext
  // transcript.
  async function onRevoke(id: string) {
    await archeionSession.revokeTagma(id);
    channelsStore.closeByTagma(id);
  }
</script>

<svelte:head><title>{tagmata_title()}</title></svelte:head>

<!-- Single scroll root (the AppShell overflow-hidden contract); the
     left-aligned narrow column matches the other manage pages. -->
<div class="h-full overflow-y-auto">
  <div class="px-2 py-4 md:p-6 max-w-2xl space-y-6">
    <h1 class="text-xl font-semibold hidden md:block">
      {nav_manage()}
    </h1>

    {#if spawnResult}
      <p class="text-sm text-success-500 dark:text-success-400">
        {manage_instances_spawn_success({
          slug: spawnResult.slug,
          port: spawnResult.port,
        })}
        <a
          class="underline underline-offset-2 ml-1"
          href={"/connect?tagmaUrl=http://127.0.0.1:" + spawnResult.port}
          >{nav_chat()}</a
        >
      </p>
      {#if pushStatus && pushStatus.slug === spawnResult.slug}
        {#if pushStatus.kind === "pending"}
          <p class="text-sm opacity-70">
            {manage_instances_push_pending()}
          </p>
        {:else if pushStatus.kind === "pushed"}
          <p
            class="text-sm {pushStatus.probeOk
              ? 'text-success-500 dark:text-success-400'
              : 'text-warning-500 dark:text-warning-400'}"
          >
            {pushStatus.probeOk
              ? manage_instances_push_verified({ slug: spawnResult.slug })
              : manage_instances_push_unverified({ slug: spawnResult.slug })}
          </p>
        {:else if pushStatus.kind === "unreachable"}
          <p class="text-sm text-warning-500 dark:text-warning-400">
            {manage_instances_push_unreachable({ slug: spawnResult.slug })}
          </p>
        {:else if pushStatus.kind === "not-applied"}
          <p class="text-sm text-warning-500 dark:text-warning-400">
            {manage_instances_push_not_applied({ slug: spawnResult.slug })}
          </p>
        {:else if pushStatus.kind === "locked"}
          <p class="text-sm text-error-500 dark:text-error-400">
            {manage_instances_create_provider_locked_hint()}
          </p>
        {:else}
          <p class="text-sm text-error-500 dark:text-error-400">
            {manage_instances_push_failed({
              slug: spawnResult.slug,
              message: pushStatus.message ?? "",
            })}
          </p>
        {/if}
      {/if}
    {/if}

    <!-- The process-host banner is non-blocking: the registry cards do not
         depend on it, so its failure costs the process rows only. -->
    {#if instancesStore.errorKind}
      {#if instancesStore.errorKind === "unauthorized" && instancesStore.errorCode === "admin_session_required"}
        <!-- Platform mode, cookie channel: no token to paste -- the
             admin login itself carries the right. -->
        <p class="text-error-500 dark:text-error-400 text-sm">
          {manage_instances_session_required()}
        </p>
      {:else if instancesStore.errorKind === "unauthorized"}
        <form class="space-y-2" onsubmit={onTokenApply}>
          <p class="text-error-500 dark:text-error-400 text-sm">
            {manage_instances_unauthorized()}
          </p>
          <div class="flex gap-2">
            <input
              class="input"
              type="password"
              autocomplete="off"
              bind:value={tokenInput}
              placeholder={manage_instances_token_placeholder()}
              aria-label={manage_instances_token_label()}
            />
            <button type="submit" class="btn preset-filled-primary-500 shrink-0"
              >{manage_instances_token_apply()}</button
            >
          </div>
          {#if tokenRejected}
            <p class="text-xs text-error-500 dark:text-error-400">
              {manage_instances_token_rejected()}
            </p>
          {/if}
        </form>
      {:else}
        <p class="text-error-500 dark:text-error-400 text-sm">
          {#if instancesStore.errorKind === "forbidden" && instancesStore.errorCode === "host_forbidden"}
            {manage_instances_host_forbidden()}
          {:else}
            {kindMessage[instancesStore.errorKind]()}
          {/if}
        </p>
      {/if}
    {/if}

    {#if archeionSession.tagmataError}
      <p class="text-error-500 dark:text-error-400 text-sm">
        {tagmata_load_failed()}
      </p>
    {/if}

    {#if startError}
      <p class="text-error-500 dark:text-error-400 text-sm">
        {startError}
      </p>
    {/if}
    {#if instancesStore.errorKind === null && !instancesStore.loaded}
      <p class="text-sm opacity-70">{manage_instances_loading()}</p>
    {:else if archeionSession.user != null && !archeionSession.tagmataLoaded && !archeionSession.tagmataError}
      <p class="text-sm opacity-70">{manage_instances_loading()}</p>
    {/if}

    <!-- Header row: the single create action. The daemon health line is
         deliberately gone -- process trouble surfaces through the error
         banner above, not a service-status readout. -->
    <div class="flex items-center justify-end gap-4 flex-wrap">
      <button
        type="button"
        class="btn preset-filled-primary-500 shrink-0"
        onclick={openCreate}
      >
        + {manage_instances_new()}
      </button>
    </div>

    {#if empty}
      <!-- First-run empty state: promote the
           single primary action, opening the two-path dialog. -->
      <div class="p-10 grid place-items-center gap-4">
        <p class="text-sm opacity-70">{manage_instances_empty()}</p>
        <button
          type="button"
          class="card preset-filled-primary-500 w-full max-w-md p-8 text-center transition hover:brightness-110"
          onclick={openCreate}
        >
          <div class="text-2xl font-semibold">{tagmata_new()}</div>
          <div class="opacity-80">{tagmata_new_hint()}</div>
        </button>
      </div>
    {:else}
      <!-- Pending cards first (time-sensitive codes), then the joined
           device cards. -->
      <div class="flex flex-col gap-3">
        {#each pending as code (code.id)}
          <EnrollmentCodeCard
            {code}
            onCopy={(id, secret) => archeionSession.copySecret(id, secret)}
            {onRevoke}
            onRename={(id, label) => archeionSession.renameTagma(id, label)}
            copied={archeionSession.copiedCodeId === code.id}
          />
        {/each}
        {#each devices as d (d.key)}
          <TagmaCard
            tagma={d.tagma}
            process={d.process}
            onRename={(id, label) => archeionSession.renameTagma(id, label)}
            {onRevoke}
            onStart={onStartClick}
            onStop={(slug) => {
              stopTarget = slug;
              stopError = null;
            }}
          />
        {/each}
      </div>
    {/if}

    {#if shellMode() === "online"}
      <!-- Rooms management section: visibility keys on the shell mode only --
           a transient rooms fetch error must not hide the
           standing /rooms entry. roomsLoaded only picks rows-vs-empty inside. -->
      <section class="space-y-3" aria-label={nav_rooms()}>
        <h2 class="text-sm font-semibold uppercase tracking-wide opacity-60">
          {nav_rooms()}
        </h2>
        <div class="card preset-tonal-surface divide-y divide-surface-200-800">
          {#if roomsStore.roomsLoaded}
            {#if roomsStore.rooms.length > 0}
              {#each roomsStore.rooms as r (r.room_id)}
                <HubRow
                  href={`/rooms/${r.room_id}`}
                  Icon={Users}
                  label={r.name ||
                    room_label_fallback({ id: r.room_id.slice(0, 8) })}
                />
              {/each}
            {:else}
              <HubRow
                href="/rooms"
                Icon={Users}
                label={tagma_rooms_section_empty()}
              />
            {/if}
          {:else if roomsStore.roomsError}
            <p class="text-error-500 dark:text-error-400 text-sm px-4 py-3">
              {roomsStore.roomsError}
            </p>
            <p class="opacity-60 text-sm px-4 pb-3">{rooms_retrying()}</p>
          {:else}
            <p class="text-sm opacity-70 px-4 py-3">{common_loading()}</p>
          {/if}
          <HubRow
            href="/rooms"
            Icon={Users}
            label={tagma_rooms_section_manage()}
          />
        </div>
      </section>
    {/if}
    <ConfirmDialog
      open={stopTarget !== null}
      title={manage_instances_stop_title()}
      description={manage_instances_stop_description({
        slug: stopTarget ?? "",
      })}
      confirmLabel={manage_instances_stop()}
      busy={stopBusy}
      tone="danger"
      error={stopError}
      onConfirm={onStopConfirmed}
      onCancel={() => {
        stopTarget = null;
        stopError = null;
      }}
    />

    <CreateInstanceDialog
      open={createOpen}
      busy={createBusy}
      error={createError}
      {canSpawn}
      providers={archeionSession.providers.map((p) => ({
        id: p.id,
        name: p.name,
        family: p.provider,
        baseUrl: p.base_url,
        mode: p.mode,
      }))}
      sessionViaPasskey={archeionSession.canFlipKeys()}
      {onOneClick}
      {onSpawn}
      {onMint}
      onCancel={() => (createOpen = false)}
    />
  </div>
</div>
