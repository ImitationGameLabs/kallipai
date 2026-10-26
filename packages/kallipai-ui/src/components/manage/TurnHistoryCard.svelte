<script lang="ts">
  // Turn-history card: the manage section's first card that fetches its own
  // data (it is mounted per agent, and a turn is only read once a reader
  // asks for it). How a face's answer is read lives in lib/manage/turnView.ts;
  // the template only picks states -- loading, a failure kind, a face it
  // cannot render -- and shows the views that layer returns; a failure's
  // copy is the card's, through lib/manage/errors.ts. What a read lands
  // in is covered there; the picking above is not.
  //
  // Two honesty boundaries are deliberate and pinned by the render test: a
  // message row offers no "read the whole message" control (the read face
  // has no message-body route, so only a call's stored text can be read),
  // and stored text is rendered as text -- no `{@html}`, no link, no URL
  // detection. The tagma already replaced spilled stream paths with a plain
  // marker and counts the replacements, which this card only shows.
  //
  // What stays here is plumbing: the era that marks a read as still
  // belonging to the agent on screen, the five raw presence checks in the
  // template (profile, effort, elapsed_ms, usage, text !== null) -- each
  // asks whether the wire carried a field at all -- and the picking of
  // which state to show.

  import type {
    TurnDetail,
    TurnRow,
    TurnsPage,
  } from "@kallipai/kallipai-client";
  import { SvelteSet } from "svelte/reactivity";
  import type { ManagementBackend } from "../../lib/manage/backend.ts";
  import { displayError } from "../../lib/manage/errors.ts";
  import {
    appendRange,
    type BodyText,
    callDetails,
    callStatus,
    canLoadMore,
    type CardError,
    emptyBodyText,
    errorState,
    errorStatus,
    faceSupported,
    mergePages,
    messageView,
    type Notice,
    pageNotices,
    reasoningView,
    stillCurrent,
    TURNS_PAGE_LIMIT,
  } from "../../lib/manage/turnView.ts";
  import { formatStamp } from "../../lib/time/stamp.svelte.ts";
  import {
    manage_turns_args_label,
    manage_turns_at_label,
    manage_turns_bad_lines,
    manage_turns_body_empty,
    manage_turns_body_end,
    manage_turns_body_loading,
    manage_turns_body_more,
    manage_turns_body_range,
    manage_turns_body_reclaimed,
    manage_turns_call_cut,
    manage_turns_call_exit,
    manage_turns_call_failed,
    manage_turns_call_ok,
    manage_turns_call_pending,
    manage_turns_call_timed_out,
    manage_turns_calls_label,
    manage_turns_collapse,
    manage_turns_effort_value,
    manage_turns_elapsed_label,
    manage_turns_elapsed_value,
    manage_turns_empty,
    manage_turns_expand,
    manage_turns_heading,
    manage_turns_hide_original,
    manage_turns_load_failed,
    manage_turns_load_more,
    manage_turns_loading,
    manage_turns_message_chars,
    manage_turns_message_cut,
    manage_turns_message_cut_note,
    manage_turns_messages_label,
    manage_turns_no_log,
    manage_turns_page_cut,
    manage_turns_profile_id,
    manage_turns_reasoning_cut,
    manage_turns_reasoning_flag,
    manage_turns_reasoning_hide,
    manage_turns_reasoning_show,
    manage_turns_reasoning_unavailable,
    manage_turns_refresh,
    manage_turns_schema_newer,
    manage_turns_show_original,
    manage_turns_stderr_line,
    manage_turns_stdout_line,
    manage_turns_turn_empty,
    manage_turns_turn_id,
    manage_turns_usage_label,
    manage_turns_usage_line,
  } from "../../paraglide/messages.js";

  let { id, backend }: { id: string; backend: ManagementBackend } = $props();

  /** What the card holds for one call's text: the folded ranges from
   * turnView, plus the states only the card knows. */
  type BodyState = BodyText & {
    loading: boolean;
    error: string | null;
    unsupported: boolean;
    /** Which read is filling this block: pure identity, never rendered. A
     * block closed and opened again carries a newer token, so an answer from
     * the read that started on the older block cannot land in it. */
    token: number;
  };

  let rows = $state<TurnRow[]>([]);
  let page = $state<TurnsPage | null>(null);
  /** Unreadable lines the pages already loaded reported; each page adds its own. */
  let earlierBadLines = $state(0);
  let listFire = $state<CardError>("none");
  let listError = $state<string | null>(null);
  let moreError = $state<string | null>(null);
  let refreshError = $state<string | null>(null);
  let loading = $state(false);
  let loadingMore = $state(false);
  let expandedId = $state<number | null>(null);
  let detail = $state<TurnDetail | null>(null);
  let detailFire = $state<CardError>("none");
  let detailError = $state<string | null>(null);
  let detailLoading = $state(false);
  let bodies = $state<Record<string, BodyState>>({});
  let loadedFor = $state<string | null>(null);
  /** Bumped whenever the card is re-pointed at another agent: a read that
   * started under an older era must not write. */
  let readEpoch = 0;
  /** Handed out one per body-range read; the block remembers the token of the
   * read filling it. */
  let bodyToken = 0;
  const openReasoning = new SvelteSet<string>();

  const notes = $derived(
    page === null ? [] : pageNotices(page, earlierBadLines),
  );
  /** Whether the list and the open turn may be rendered at all: a newer face
   * is announced and not read, the same stop the CLI makes. */
  const listSupported = $derived(
    page === null ? true : faceSupported(page.schema_version),
  );
  const detailSupported = $derived(
    detail === null ? true : faceSupported(detail.schema_version),
  );

  // One read per agent: a deep link that swaps the id resets the card instead
  // of showing one agent's turns under another's name, and the era bump makes
  // any read still in flight drop its answer on arrival.
  $effect(() => {
    if (loadedFor === id) return;
    loadedFor = id;
    readEpoch += 1;
    rows = [];
    page = null;
    earlierBadLines = 0;
    listFire = "none";
    listError = null;
    moreError = null;
    refreshError = null;
    expandedId = null;
    detail = null;
    detailFire = "none";
    detailError = null;
    bodies = {};
    // The three in-flight flags describe what this card was doing for the
    // agent that just went away, and a dropped answer never reaches its
    // finally: reset them here or they stay stuck on.
    loading = false;
    loadingMore = false;
    detailLoading = false;
    openReasoning.clear();
    void loadFirst();
  });

  async function loadFirst() {
    // One read of this list at a time: a refresh and a paging ask both read
    // pages, and two in flight would interleave their answers.
    if (loading || loadingMore) return;
    const asked = { agent: id, epoch: readEpoch };
    loading = true;
    listError = null;
    moreError = null;
    refreshError = null;
    try {
      const first = await backend.turns(id, { limit: TURNS_PAGE_LIMIT });
      if (!stillCurrent(asked, { agent: id, epoch: readEpoch })) return;
      earlierBadLines = 0;
      page = first;
      rows = mergePages([], first);
      listFire = "none";
    } catch (e) {
      if (!stillCurrent(asked, { agent: id, epoch: readEpoch })) return;
      const message = displayError(
        "turn-history",
        e,
        manage_turns_load_failed(),
      );
      const fire = errorState("list", errorStatus(e));
      // A failed refresh keeps the rows already read: only a first read has
      // no list to fall back on. The message sits under the header either
      // way -- at the bottom it would read as a paging failure.
      if (rows.length === 0) {
        listFire = fire;
        listError = message;
      } else {
        refreshError = message;
      }
    } finally {
      if (stillCurrent(asked, { agent: id, epoch: readEpoch })) {
        loading = false;
      }
    }
  }

  async function loadMore() {
    // One read of this list at a time: a refresh and a paging ask both read
    // pages, and two in flight would interleave their answers.
    if (loading || loadingMore) return;
    const cursor = page?.next_cursor;
    if (cursor === undefined) return;
    const asked = { agent: id, epoch: readEpoch };
    loadingMore = true;
    moreError = null;
    try {
      const next = await backend.turns(id, {
        limit: TURNS_PAGE_LIMIT,
        before: cursor,
      });
      if (!stillCurrent(asked, { agent: id, epoch: readEpoch })) return;
      earlierBadLines += page?.bad_lines ?? 0;
      rows = mergePages(rows, next);
      page = next;
    } catch (e) {
      if (!stillCurrent(asked, { agent: id, epoch: readEpoch })) return;
      moreError = displayError("turn-history", e, manage_turns_load_failed());
    } finally {
      if (stillCurrent(asked, { agent: id, epoch: readEpoch })) {
        loadingMore = false;
      }
    }
  }

  async function toggleTurn(turnId: number) {
    if (expandedId === turnId) {
      expandedId = null;
      return;
    }
    const asked = { agent: id, epoch: readEpoch };
    expandedId = turnId;
    detail = null;
    detailFire = "none";
    detailError = null;
    bodies = {};
    openReasoning.clear();
    detailLoading = true;
    try {
      const read = await backend.turn(id, turnId);
      // A second click, or a swap to another agent, may have moved on while
      // this read was in flight.
      if (
        expandedId !== turnId ||
        !stillCurrent(asked, { agent: id, epoch: readEpoch })
      ) {
        return;
      }
      detail = read;
    } catch (e) {
      if (
        expandedId !== turnId ||
        !stillCurrent(asked, { agent: id, epoch: readEpoch })
      ) {
        return;
      }
      detailFire = errorState("detail", errorStatus(e));
      detailError = displayError("turn-history", e, manage_turns_load_failed());
    } finally {
      if (
        expandedId === turnId &&
        stillCurrent(asked, { agent: id, epoch: readEpoch })
      ) {
        detailLoading = false;
      }
    }
  }

  function noticeText(notice: Notice): string {
    switch (notice.kind) {
      case "bad-lines":
        return manage_turns_bad_lines({ count: notice.count });
      case "page-cut":
        return manage_turns_page_cut();
      case "schema-newer":
        return manage_turns_schema_newer();
      default: {
        // A new kind without a line here is a compile error, not a blank row.
        const unread: never = notice;
        return unread;
      }
    }
  }

  function statusText(status: "ok" | "pending" | "failed"): string {
    if (status === "ok") return manage_turns_call_ok();
    if (status === "pending") return manage_turns_call_pending();
    return manage_turns_call_failed();
  }

  /** One row inside a turn: a message and a call share the index space. */
  function rowKey(turnId: number, index: number): string {
    return `${turnId}:${index}`;
  }

  function reasoningOpen(turnId: number, index: number): boolean {
    return openReasoning.has(rowKey(turnId, index));
  }

  function toggleReasoning(turnId: number, index: number) {
    const key = rowKey(turnId, index);
    if (openReasoning.has(key)) openReasoning.delete(key);
    else openReasoning.add(key);
  }

  /** A block asked for but not read yet: no text, loading. */
  function pendingBody(): BodyState {
    return {
      ...emptyBodyText(),
      loading: true,
      error: null,
      unsupported: false,
      token: 0,
    };
  }

  async function toggleBody(turnId: number, index: number) {
    const key = rowKey(turnId, index);
    if (bodies[key] !== undefined) {
      const next = { ...bodies };
      delete next[key];
      bodies = next;
      return;
    }
    await readRange(turnId, index, undefined, false);
  }

  async function readRange(
    turnId: number,
    index: number,
    offset: number | undefined,
    append: boolean,
  ) {
    const key = rowKey(turnId, index);
    bodyToken += 1;
    const token = bodyToken;
    const previous = bodies[key] ?? pendingBody();
    bodies = {
      ...bodies,
      [key]: { ...previous, token, loading: true, error: null },
    };
    // One question decides whether this answer still belongs here: is the
    // block under this key still the one this read started on? A newer token
    // covers a closed block, a block closed and opened again, and a swap to
    // another agent, which empties the map -- and it keeps an older append
    // read from showing a mid-range slice as a whole text.
    const mine = () => bodies[key]?.token === token;
    try {
      const read = await backend.callBody(id, turnId, index, offset);
      if (!mine()) return;
      if (!faceSupported(read.schema_version)) {
        bodies = {
          ...bodies,
          [key]: {
            ...previous,
            token,
            loading: false,
            error: null,
            unsupported: true,
          },
        };
        return;
      }
      const text = append
        ? appendRange(previous, read)
        : appendRange(null, read);
      bodies = {
        ...bodies,
        [key]: {
          ...text,
          token,
          loading: false,
          error: null,
          unsupported: false,
        },
      };
    } catch (e) {
      if (!mine()) return;
      bodies = {
        ...bodies,
        [key]: {
          ...previous,
          token,
          loading: false,
          error: displayError("turn-history", e, manage_turns_load_failed()),
        },
      };
    }
  }
</script>

<section class="card preset-tonal-surface p-5 space-y-3">
  <div class="flex items-center justify-between gap-2">
    <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
      {manage_turns_heading()}
    </h2>
    <button
      type="button"
      class="text-xs opacity-60 hover:opacity-100 transition"
      disabled={loading || loadingMore}
      onclick={loadFirst}>{manage_turns_refresh()}</button
    >
  </div>

  {#if refreshError}
    <p class="text-xs text-error-500">{refreshError}</p>
  {/if}

  {#if loading && page === null}
    <p class="text-xs opacity-70">{manage_turns_loading()}</p>
  {:else if listFire === "no-log"}
    <p class="text-xs opacity-70">{manage_turns_no_log()}</p>
  {:else if listFire === "failed"}
    <p class="text-xs text-error-500">
      {listError ?? manage_turns_load_failed()}
    </p>
    <button
      type="button"
      class="text-xs opacity-60 hover:opacity-100 transition"
      disabled={loading || loadingMore}
      onclick={loadFirst}>{manage_turns_refresh()}</button
    >
  {:else if !listSupported}
    {#each notes as notice}
      <p class="text-xs opacity-60">{noticeText(notice)}</p>
    {/each}
  {:else if rows.length === 0}
    <p class="text-xs opacity-70">{manage_turns_empty()}</p>
  {:else}
    {#each notes as notice}
      <p class="text-xs opacity-60">{noticeText(notice)}</p>
    {/each}
    <ul class="space-y-2">
      {#each rows as row (row.turn_id)}
        <li class="rounded border border-surface-200-800 p-3 space-y-2">
          <div class="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
            <span class="font-mono font-medium"
              >{manage_turns_turn_id({ id: row.turn_id })}</span
            >
            <span class="opacity-70"
              >{manage_turns_at_label()}: {formatStamp(row.at)}</span
            >
            {#if row.profile}
              <span class="opacity-70"
                >{row.profile.provider} · {row.profile.model}</span
              >
              <span class="opacity-60 font-mono"
                >{manage_turns_profile_id({
                  id: row.profile.profile_id,
                })}</span
              >
              {#if row.profile.effort}
                <span class="opacity-60"
                  >{manage_turns_effort_value({
                    effort: row.profile.effort,
                  })}</span
                >
              {/if}
            {/if}
            {#if row.elapsed_ms !== undefined}
              <span class="opacity-60"
                >{manage_turns_elapsed_label()}:
                {manage_turns_elapsed_value({ ms: row.elapsed_ms })}</span
              >
            {/if}
            <span class="opacity-60"
              >{manage_turns_messages_label()}: {row.messages.length}</span
            >
            <span class="opacity-60"
              >{manage_turns_calls_label()}: {row.calls.length}</span
            >
          </div>
          {#if row.usage}
            <p class="text-xs opacity-60">
              {manage_turns_usage_label()}:
              {manage_turns_usage_line({
                prompt: row.usage.prompt,
                completion: row.usage.completion,
                cacheRead: row.usage.cache_read,
                cacheWrite: row.usage.cache_write,
                reasoning: row.usage.reasoning,
              })}
            </p>
          {/if}
          <button
            type="button"
            class="text-xs opacity-60 hover:opacity-100 transition"
            onclick={() => toggleTurn(row.turn_id)}
          >
            {expandedId === row.turn_id
              ? manage_turns_collapse()
              : manage_turns_expand()}
          </button>
          {#if expandedId === row.turn_id}
            {#if detailLoading}
              <p class="text-xs opacity-70">{manage_turns_loading()}</p>
            {:else if detailFire === "failed"}
              <p class="text-xs text-error-500">
                {detailError ?? manage_turns_load_failed()}
              </p>
            {:else if !detailSupported}
              <p class="text-xs opacity-60">{manage_turns_schema_newer()}</p>
            {:else if detail}
              {@const turn = detail.turn}
              <div class="space-y-2">
                {#if turn.messages.length === 0 && turn.calls.length === 0}
                  <p class="text-xs opacity-70">{manage_turns_turn_empty()}</p>
                {/if}
                {#if turn.messages.length > 0}
                  <p class="text-xs opacity-60 uppercase tracking-wide">
                    {manage_turns_messages_label()}
                  </p>
                {/if}
                {#each turn.messages as message, index}
                  {@const view = messageView(message)}
                  {@const thought = reasoningView(message)}
                  <div class="rounded bg-surface-100-900 p-2 space-y-1">
                    <div
                      class="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs opacity-70"
                    >
                      <span class="font-mono">{message.role}</span>
                      <span
                        >{manage_turns_message_chars({
                          chars: view.chars,
                        })}</span
                      >
                      {#if view.cut}
                        <span>{manage_turns_message_cut()}</span>
                      {/if}
                      {#if thought.state !== "none"}
                        <span>{manage_turns_reasoning_flag()}</span>
                      {/if}
                    </div>
                    {#if view.text !== null}
                      <p class="text-xs whitespace-pre-wrap break-words">
                        {view.text}
                      </p>
                    {/if}
                    {#if view.cut}
                      <p class="text-xs opacity-60">
                        {manage_turns_message_cut_note({ chars: view.chars })}
                      </p>
                    {/if}
                    {#if thought.state === "text"}
                      <button
                        type="button"
                        class="text-xs opacity-60 hover:opacity-100 transition"
                        onclick={() => toggleReasoning(turn.turn_id, index)}
                      >
                        {reasoningOpen(turn.turn_id, index)
                          ? manage_turns_reasoning_hide()
                          : manage_turns_reasoning_show()}
                      </button>
                      {#if reasoningOpen(turn.turn_id, index)}
                        <p
                          class="text-xs whitespace-pre-wrap break-words opacity-80"
                        >
                          {thought.text}
                        </p>
                        {#if thought.cut}
                          <p class="text-xs opacity-60">
                            {manage_turns_reasoning_cut()}
                          </p>
                        {/if}
                      {/if}
                    {:else if thought.state === "unavailable"}
                      <p class="text-xs opacity-60">
                        {manage_turns_reasoning_unavailable()}
                      </p>
                    {/if}
                  </div>
                {/each}

                {#if turn.calls.length > 0}
                  <p class="text-xs opacity-60 uppercase tracking-wide">
                    {manage_turns_calls_label()}
                  </p>
                {/if}
                {#each turn.calls as call}
                  {@const status = callStatus(call)}
                  {@const facts = callDetails(call)}
                  {@const key = rowKey(turn.turn_id, call.call_index)}
                  <div class="rounded bg-surface-100-900 p-2 space-y-1">
                    <div
                      class="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs"
                    >
                      <span class="font-mono">#{call.call_index}</span>
                      <span class="font-mono">{call.name}</span>
                      <span class="opacity-70">{statusText(status)}</span>
                      {#if facts.exit !== null}
                        <span class="opacity-60"
                          >{manage_turns_call_exit({ code: facts.exit })}</span
                        >
                      {/if}
                      {#if facts.timedOut}
                        <span class="opacity-60"
                          >{manage_turns_call_timed_out()}</span
                        >
                      {/if}
                      {#if facts.cut}
                        <span class="opacity-60">{manage_turns_call_cut()}</span
                        >
                      {/if}
                    </div>
                    {#if facts.stdout}
                      <p class="text-xs opacity-60">
                        {manage_turns_stdout_line(facts.stdout)}
                      </p>
                    {/if}
                    {#if facts.stderr}
                      <p class="text-xs opacity-60">
                        {manage_turns_stderr_line(facts.stderr)}
                      </p>
                    {/if}
                    {#if facts.firstError}
                      <p class="text-xs font-mono opacity-80">
                        {facts.firstError}
                      </p>
                    {/if}
                    <div>
                      <span class="text-xs opacity-60 uppercase tracking-wide"
                        >{manage_turns_args_label()}</span
                      >
                      <p class="text-xs font-mono truncate" title={call.args}>
                        {call.args}
                      </p>
                    </div>
                    <button
                      type="button"
                      class="text-xs opacity-60 hover:opacity-100 transition"
                      onclick={() => toggleBody(turn.turn_id, call.call_index)}
                    >
                      {bodies[key] !== undefined
                        ? manage_turns_hide_original()
                        : manage_turns_show_original()}
                    </button>
                    {#if bodies[key]}
                      {@const shown = bodies[key]}
                      <div class="space-y-1">
                        {#if shown.loading}
                          <p class="text-xs opacity-60">
                            {manage_turns_body_loading()}
                          </p>
                        {:else if shown.error}
                          <p class="text-xs text-error-500">{shown.error}</p>
                        {:else if shown.unsupported}
                          <p class="text-xs opacity-60">
                            {manage_turns_schema_newer()}
                          </p>
                        {:else if shown.empty}
                          <p class="text-xs opacity-60">
                            {manage_turns_body_empty()}
                          </p>
                        {:else}
                          <p class="text-xs opacity-60">
                            {manage_turns_body_range({
                              from: shown.from,
                              to: shown.to,
                              total: shown.total,
                            })}
                          </p>
                          <pre
                            class="text-xs font-mono whitespace-pre-wrap break-words max-h-64 overflow-auto">{shown.text}</pre>
                          {#if shown.reclaimed > 0}
                            <p class="text-xs opacity-60">
                              {manage_turns_body_reclaimed({
                                count: shown.reclaimed,
                              })}
                            </p>
                          {/if}
                          {#if shown.atEnd}
                            <p class="text-xs opacity-60">
                              {manage_turns_body_end()}
                            </p>
                          {:else if shown.more !== null}
                            <button
                              type="button"
                              class="text-xs opacity-60 hover:opacity-100 transition"
                              onclick={() =>
                                readRange(
                                  turn.turn_id,
                                  call.call_index,
                                  shown.more ?? undefined,
                                  true,
                                )}
                            >
                              {manage_turns_body_more()}
                            </button>
                          {:else}
                            <button
                              type="button"
                              class="text-xs opacity-60 hover:opacity-100 transition"
                              onclick={() =>
                                readRange(
                                  turn.turn_id,
                                  call.call_index,
                                  undefined,
                                  false,
                                )}
                            >
                              {manage_turns_refresh()}
                            </button>
                          {/if}
                        {/if}
                      </div>
                    {/if}
                  </div>
                {/each}
              </div>
            {/if}
          {/if}
        </li>
      {/each}
    </ul>
    {#if moreError}
      <p class="text-xs text-error-500">{moreError}</p>
    {/if}
    {#if canLoadMore(page, loadingMore)}
      <button
        type="button"
        class="text-xs opacity-60 hover:opacity-100 transition"
        disabled={loading || loadingMore}
        onclick={loadMore}>{manage_turns_load_more()}</button
      >
    {/if}
  {/if}
</section>
