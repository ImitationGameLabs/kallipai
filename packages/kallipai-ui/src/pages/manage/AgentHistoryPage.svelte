<script lang="ts">
  // One agent's turn history as its own page: the turn list beside the
  // selected turn on a wide screen, one of the two on a narrow screen.
  //
  // Every read lives here -- the list pages, the turn detail, and each call's
  // stored text -- because each one has to answer "is this answer still mine"
  // before it may write. The components below take props and callbacks and
  // read nothing themselves.
  //
  // The roster read exists only to label the heading with the agent's role.
  // It is allowed to fail: the id is a fine label, and a page about turns
  // should not refuse to open because a name lookup did.
  import type {
    CallPart,
    TextPart,
    TurnDetail as TurnRecord,
    TurnRow,
    TurnsPage,
  } from "@kallipai/kallipai-client";
  import { tick } from "svelte";
  import type { ManagementBackend } from "../../lib/manage/backend.ts";
  import { managementBackend } from "../../lib/manage/client.ts";
  import { displayError } from "../../lib/manage/errors.ts";
  import { lgQuery } from "../../lib/shell/breakpoint.ts";
  import CopyButton from "../../components/CopyButton.svelte";
  import TurnDetail from "../../components/manage/TurnDetail.svelte";
  import TurnList from "../../components/manage/TurnList.svelte";
  import {
    appendRange,
    type BodyEntry,
    type BodyOutcome,
    bodyKey,
    canLoadMore,
    type CardError,
    errorState,
    errorStatus,
    faceSupported,
    mergePages,
    pageNotices,
    showOnRows,
    showOnWidth,
    type ShownTurn,
    stillCurrent,
    type TextRange,
    TURNS_PAGE_LIMIT,
    untouchedResults,
  } from "../../lib/manage/turnView.ts";
  import { BlockTexts } from "../../lib/manage/blockTexts.svelte.ts";
  import {
    manage_agent_back,
    manage_turns_back,
    manage_turns_body_break,
    manage_turns_body_wrong_message,
    manage_turns_load_failed,
    manage_turns_page_title,
  } from "../../paraglide/messages.js";

  let {
    id,
    basePath = "/local/manage",
    backend = managementBackend(),
  }: {
    id: string;
    basePath?: string;
    /** Single injected source: local callers fall back to the offline
     * backend; the online route injects a tagma-resolved OnlineBackend so a
     * deep link renders without store switching. */
    backend?: ManagementBackend;
  } = $props();

  let role = $state<string | null>(null);
  /** Wide enough for both columns: the fork at 64rem; both widths are
   * declared in lib/shell/breakpoint.ts. Below it the page keeps one
   * scroller. Read once at mount and kept current by the media query. */
  const widePanes = matchMedia(lgQuery);
  let panesSideBySide = $state(widePanes.matches);
  /** Whether the layout, not the reader, chose the turn on show: the pick is
   * dropped when the room for it goes away. Three places write it, each for
   * its own reason: a click of the reader's clears it, clearing the selection
   * clears it, and a decision's mark is written as that decision lands. */
  let autoPicked = $state(false);
  /** The column that scrolls one turn on a wide screen, and the page root that
   * scrolls the stacked layout: a selection sends whichever one is in charge
   * back to its top. */
  let detailPane = $state<HTMLDivElement | null>(null);
  let pageRoot = $state<HTMLDivElement | null>(null);
  /** The way back to the list, which takes the focus when a keyboard pick
   * happens on a stacked layout: the row that was focused is now hidden. */
  let backToList = $state<HTMLButtonElement | null>(null);

  // --- the list ---
  let rows = $state<TurnRow[]>([]);
  let page = $state<TurnsPage | null>(null);
  /** Unreadable lines the pages already loaded reported; each page adds its
   * own. */
  let earlierBadLines = $state(0);
  let listFire = $state<CardError>("none");
  let listError = $state<string | null>(null);
  let moreError = $state<string | null>(null);
  let refreshError = $state<string | null>(null);
  let loading = $state(false);
  let loadingMore = $state(false);

  // --- the selected turn ---
  /** How a turn was picked: from the keyboard, whose focus has to go
   * somewhere once the list hides; and whether to read a turn already on
   * show again. */
  type PickOptions = { viaKeyboard?: boolean; force?: boolean };
  let selectedId = $state<number | null>(null);
  let detail = $state<TurnRecord | null>(null);
  let detailFire = $state<CardError>("none");
  let detailError = $state<string | null>(null);
  let detailLoading = $state(false);
  /** Names the detail read that owns the pane; only ever grows, so a read for
   * a turn chosen again cannot land its older answer over a newer one. */
  let detailToken = 0;

  // --- call texts ---
  /** The stored text of every call block. The module owns the writes: the
   * page asks for a read and draws what comes back. */
  const texts = new BlockTexts();
  /** Bumped whenever the page is re-pointed at another agent: a read that
   * started under an older era must not write. */
  let readEpoch = 0;
  let loadedFor = $state<string | null>(null);

  const notes = $derived(
    page === null ? [] : pageNotices(page, earlierBadLines),
  );
  /** The face number a read announced when this build cannot read it, or
   * null when it can. The page stops there, as the CLI does, and the list says
   * so in one line: the number is the reason, not something the reader reads. */
  const listFace = $derived(
    page === null || faceSupported(page.schema_version)
      ? null
      : page.schema_version,
  );
  const detailFace = $derived(
    detail === null || faceSupported(detail.schema_version)
      ? null
      : detail.schema_version,
  );
  const showList = $derived(panesSideBySide || selectedId === null);
  const showDetail = $derived(panesSideBySide || selectedId !== null);

  $effect(() => {
    const onChange = (event: MediaQueryListEvent) => {
      panesSideBySide = event.matches;
      applyShownTurn(
        showOnWidth({ panes: panesSideBySide, shown: shownTurn(), rows }),
      );
    };
    widePanes.addEventListener("change", onChange);
    return () => widePanes.removeEventListener("change", onChange);
  });

  /** Everything a selection owns, cleared in one place: the turn on show, who
   * chose it, and the state of the read that fills it. A second caller with
   * its own list is how one of them ends up forgetting `detailLoading` and
   * leaving a dropped read's flag on. */
  function clearSelection(): void {
    selectedId = null;
    detail = null;
    autoPicked = false;
    detailLoading = false;
    detailFire = "none";
    detailError = null;
  }

  /** The turn on show, as the decision layer reads it: which turn, and whether
   * the layout chose it. */
  function shownTurn(): ShownTurn {
    return { turnId: selectedId, auto: autoPicked };
  }

  /** Apply a decision about the turn on show: the mark a decision hands back is
   * written here, so the crossing back to one column knows whose pick it is
   * dropping. */
  function applyShownTurn(next: ShownTurn): void {
    if (next.turnId !== selectedId) {
      if (next.turnId === null) clearSelection();
      else void openTurn(next.turnId);
    }
    autoPicked = next.auto;
  }

  // The roster read: a label, nothing more. A failure leaves the id.
  $effect(() => {
    let cancelled = false;
    backend
      .listAgents()
      .then((page) => {
        if (cancelled) return;
        const match = page.agents.find((a) => a.id === id);
        role = match?.role ?? null;
      })
      .catch(() => {
        if (!cancelled) role = null;
      });
    return () => {
      cancelled = true;
    };
  });

  // One read per agent: a deep link that swaps the id resets the page instead
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
    clearSelection();
    texts.clear();
    // What the page was doing for the agent that just went away: a dropped
    // answer never reaches its finally, so the flags reset here or they stay
    // stuck on. The turn pane's own flags belong to `clearSelection`.
    loading = false;
    loadingMore = false;
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
      const first = await backend.history(id, { limit: TURNS_PAGE_LIMIT });
      if (!stillCurrent(asked, { agent: id, epoch: readEpoch })) return;
      earlierBadLines = 0;
      page = first;
      rows = mergePages([], first);
      listFire = "none";
      // An empty list has nothing to show beside it, so the pane clears on
      // either layout. A stacked page also clears a chosen turn that is no
      // longer listed: the list is what shows there. A wide page keeps its
      // choice until the reader picks another -- a refreshed list may simply
      // not have reached that turn.
      const stillListed =
        selectedId !== null && rows.some((row) => row.turn_id === selectedId);
      if (rows.length === 0) {
        clearSelection();
      } else if (!panesSideBySide && selectedId !== null && !stillListed) {
        clearSelection();
      }
      applyShownTurn(
        showOnRows({ panes: panesSideBySide, shown: shownTurn(), rows }),
      );
    } catch (e) {
      if (!stillCurrent(asked, { agent: id, epoch: readEpoch })) return;
      const message = displayError(
        "turn-history",
        e,
        manage_turns_load_failed(),
      );
      const fire = errorState("list", errorStatus(e));
      // A failed refresh keeps the rows already read: only a first read has
      // no list to fall back on. The message sits above the list either way
      // -- at the bottom it would read as a paging failure.
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
      const next = await backend.history(id, {
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

  /** A chosen turn starts at its own first line: the wide layout scrolls the
   * turn column, the stacked one scrolls the page. The detail is already
   * cleared by the caller, so the top shows the loading line, not the turn
   * that was there before. */
  function resetReaderTop(): void {
    if (panesSideBySide) detailPane?.scrollTo({ top: 0 });
    else pageRoot?.scrollTo({ top: 0 });
  }

  /** Back to the list on a stacked page: the row the reader came from takes
   * the focus and is brought back into view, so the keyboard carries on from
   * where the pick left it instead of starting over at the body. */
  async function backToRow(turnId: number | null): Promise<void> {
    clearSelection();
    await tick();
    if (turnId === null) return;
    const row = pageRoot?.querySelector<HTMLButtonElement>(
      `button[data-turn="${turnId}"]`,
    );
    if (row === null || row === undefined) return;
    row.focus();
    row.scrollIntoView({ block: "nearest" });
  }

  /** A pick from the reader. The turn already on show takes no second read:
   * the pane goes back to its first line, which is what a second click on
   * that row is asking for. `force` is for a reader asking for the text again
   * after a failed read. */
  function pickTurn(turnId: number, options: PickOptions = {}): void {
    const { viaKeyboard = false, force = false } = options;
    autoPicked = false;
    if (!force && selectedId === turnId) {
      resetReaderTop();
      return;
    }
    void openTurn(turnId, { viaKeyboard });
  }

  async function openTurn(
    turnId: number,
    { viaKeyboard = false }: PickOptions = {},
  ) {
    const asked = { agent: id, epoch: readEpoch };
    const token = (detailToken += 1);
    selectedId = turnId;
    detail = null;
    resetReaderTop();
    if (!panesSideBySide && viaKeyboard) {
      void tick().then(() => {
        if (selectedId === turnId) backToList?.focus();
      });
    }
    detailFire = "none";
    detailError = null;
    texts.clear();
    detailLoading = true;
    try {
      const read = await backend.turn(id, turnId);
      // A second click, or a swap to another agent, may have moved on while
      // this read was in flight.
      if (
        detailToken !== token ||
        selectedId !== turnId ||
        !stillCurrent(asked, { agent: id, epoch: readEpoch })
      ) {
        return;
      }
      detail = read;
      // The result of each call is what this pane is opened for, so the
      // first few are read with the turn. Each read is stamped by the block
      // it fills, so switching turns leaves an older answer nowhere to land,
      // and one that fails stays a retry the reader can press.
      for (const callIndex of untouchedResults(read.turn, texts.entries)) {
        readBody(turnId, callIndex, "result", 0);
      }
    } catch (e) {
      if (
        detailToken !== token ||
        selectedId !== turnId ||
        !stillCurrent(asked, { agent: id, epoch: readEpoch })
      ) {
        return;
      }
      detailFire = errorState("detail", errorStatus(e));
      detailError = displayError("turn-history", e, manage_turns_load_failed());
    } finally {
      if (
        detailToken === token &&
        selectedId === turnId &&
        stillCurrent(asked, { agent: id, epoch: readEpoch })
      ) {
        detailLoading = false;
      }
    }
  }

  /** One call's part, from the byte the block has drawn to its end: a first
   * read asked from zero would hand back the prefix already on screen. */
  function readBody(
    turnId: number,
    callIndex: number,
    part: CallPart,
    from: number,
  ): void {
    void readRange(
      bodyKey(turnId, callIndex, part),
      (offset) => backend.callBody(id, turnId, callIndex, { part, offset }),
      from,
      false,
    );
  }

  function moreBody(
    turnId: number,
    callIndex: number,
    part: CallPart,
    offset: number,
  ): void {
    void readRange(
      bodyKey(turnId, callIndex, part),
      (ask) => backend.callBody(id, turnId, callIndex, { part, offset: ask }),
      offset,
      true,
    );
  }

  function readMessage(
    turnId: number,
    messageIndex: number,
    part: TextPart,
    role: string,
    from: number,
  ): void {
    void readRange(
      bodyKey(turnId, messageIndex, part),
      (offset) =>
        backend.messageBody(id, turnId, messageIndex, { part, offset }),
      from,
      false,
      role,
    );
  }

  function moreMessage(
    turnId: number,
    messageIndex: number,
    part: TextPart,
    offset: number,
    role: string,
  ): void {
    void readRange(
      bodyKey(turnId, messageIndex, part),
      (ask) =>
        backend.messageBody(id, turnId, messageIndex, { part, offset: ask }),
      offset,
      true,
      role,
    );
  }

  /** What one finished read amounts to: a face this build cannot read, a
   * range belonging to another message, a range that does not continue what
   * is shown -- or the text itself. */
  function readOutcome(
    read: TextRange & { schema_version: number; role?: string },
    previous: BodyEntry | null,
    append: boolean,
    expectRole: string | undefined,
  ): BodyOutcome {
    if (!faceSupported(read.schema_version)) {
      return { kind: "unsupported", face: read.schema_version };
    }
    if (expectRole !== undefined && read.role !== expectRole) {
      // The text came back for another message than the one on screen: the
      // index moved under the reader, so the text would be mislabelled.
      return { kind: "failed", error: manage_turns_body_wrong_message() };
    }
    const text = appendRange(append ? previous : null, read);
    if (text === null) {
      return { kind: "failed", error: manage_turns_body_break() };
    }
    return { kind: "text", text };
  }

  /** One range read, whichever face it comes from: the key names the slot,
   * the ask names the read. A page read settles an answer into the entry its
   * own token stamped, so a newer read or another agent leaves it nowhere. */
  async function readRange(
    key: string,
    ask: (
      offset: number,
    ) => Promise<TextRange & { schema_version: number; role?: string }>,
    offset: number,
    append: boolean,
    expectRole?: string,
  ): Promise<void> {
    const previous = texts.entries[key] ?? null;
    const token = texts.begin(key);
    try {
      const read = await ask(offset);
      texts.settle(key, token, readOutcome(read, previous, append, expectRole));
    } catch (e) {
      texts.settle(key, token, {
        kind: "failed",
        error: displayError("turn-history", e, manage_turns_load_failed()),
      });
    }
  }
</script>

<svelte:head
  ><title>{manage_turns_page_title({ agent: role || id })}</title></svelte:head
>

<div
  class="flex h-full flex-col overflow-y-auto lg:overflow-hidden"
  bind:this={pageRoot}
>
  <div
    class="flex min-h-0 flex-col gap-4 px-2 py-4 lg:flex-1 lg:overflow-hidden lg:p-6"
  >
    <div class="lg:shrink-0">
      <div class="flex items-center gap-3">
        <a
          href={`${basePath}/agents/${id}`}
          class="btn btn-sm preset-outlined-surface-500"
          >{manage_agent_back()}</a
        >
        <h1 class="min-w-0 truncate text-xl font-semibold">{role || id}</h1>
      </div>
      {#if role}
        <div class="group mt-1 flex items-start gap-1">
          <p class="min-w-0 select-text break-all font-mono text-xs opacity-60">
            {id}
          </p>
          <CopyButton getText={() => id} />
        </div>
      {/if}
    </div>

    <div
      class="lg:grid lg:min-h-0 lg:flex-1 lg:grid-cols-[15rem_minmax(0,1fr)] lg:gap-4"
    >
      <div
        class:hidden={!showList}
        class="space-y-2 lg:min-h-0 lg:overflow-y-auto lg:overscroll-contain"
      >
        <TurnList
          {rows}
          {notes}
          {selectedId}
          {loading}
          fire={listFire}
          error={listError}
          face={listFace}
          {moreError}
          canMore={canLoadMore(page, loadingMore)}
          {loadingMore}
          {refreshError}
          onSelect={(turnId, viaKeyboard) => pickTurn(turnId, { viaKeyboard })}
          onMore={() => void loadMore()}
          onRetry={() => void loadFirst()}
          onRefresh={() => void loadFirst()}
        />
      </div>

      <div
        class:hidden={!showDetail}
        class="min-w-0 space-y-2 lg:min-h-0 lg:overflow-y-auto lg:overscroll-contain"
        bind:this={detailPane}
      >
        {#if selectedId !== null}
          <button
            type="button"
            class="text-xs opacity-60 transition hover:opacity-100 lg:hidden"
            bind:this={backToList}
            onclick={() => void backToRow(selectedId)}
          >
            {manage_turns_back()}
          </button>
        {/if}
        <TurnDetail
          {detail}
          loading={detailLoading || (loading && rows.length === 0)}
          fire={detailFire}
          error={detailError}
          face={detailFace}
          empty={rows.length === 0 &&
            listFire === "none" &&
            !loading &&
            listFace === null}
          bodies={texts.entries}
          onReadMessage={readMessage}
          onMoreMessage={moreMessage}
          onReadBody={readBody}
          onRetry={() => {
            if (selectedId !== null) pickTurn(selectedId, { force: true });
          }}
          onMoreBody={moreBody}
        />
      </div>
    </div>
  </div>
</div>
