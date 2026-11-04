// Source-read pins for the turn-history page and the pieces it draws. These
// are template contracts a typecheck cannot see: which state a given answer
// shows, the two honesty boundaries the read face imposes (stored text is
// shown as text; a message row has no full-read control, since no such route
// exists), one visible branch per empty, failed and paged state, and the
// guards that decide whether an answer still belongs before it writes.
//
// The decisions themselves are covered by the unit tests of the two pure
// modules; what is pinned here is the wiring between them and the template.

import { assert, assertEquals } from "@std/assert";

const PAGE = new URL(
  "../../pages/manage/AgentHistoryPage.svelte",
  import.meta.url,
);
const LIST = new URL("./TurnList.svelte", import.meta.url);
const DETAIL = new URL("./TurnDetail.svelte", import.meta.url);
const CALL = new URL("./TurnCall.svelte", import.meta.url);
const BODY = new URL("./CallBody.svelte", import.meta.url);
const STORED = new URL("./StoredText.svelte", import.meta.url);
const BLOCK = new URL("./BlockText.svelte", import.meta.url);
const STREAM = new URL("./OutputStream.svelte", import.meta.url);
const TERMINAL_VIEW = new URL("./CallTerminal.svelte", import.meta.url);
const BREAK_VIEW = new URL("./CallBreak.svelte", import.meta.url);
const BG_READ_VIEW = new URL("./CallBgRead.svelte", import.meta.url);
const EVICT_VIEW = new URL("./CallEvict.svelte", import.meta.url);
const CONTEXT_OP_VIEW = new URL("./CallContextOp.svelte", import.meta.url);
const REGISTRY = new URL("./CallViews.svelte", import.meta.url);
const TREE = new URL("./JsonView.svelte", import.meta.url);
const BREAK = new URL("../../lib/shell/breakpoint.ts", import.meta.url);
const AGENT = new URL(
  "../../pages/manage/AgentDetailPage.svelte",
  import.meta.url,
);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

/** A component's source between two markers, so a pin can speak of one path. */
function between(src: string, from: string, to: string): string {
  const start = src.indexOf(from);
  const end = src.indexOf(to, start + 1);
  assert(start > 0, `${from} must appear in the source`);
  assert(end > start, `${to} must follow ${from} in the source`);
  return src.slice(start, end);
}

function count(hay: string, needle: string): number {
  return hay.split(needle).length - 1;
}

Deno.test(
  "a call's stored text is drawn as text, never injected as markup",
  { permissions: { read: [CALL, BODY, PAGE, DETAIL, BLOCK, STREAM, STORED] } },
  () => {
    for (const url of [CALL, BODY, DETAIL, PAGE, BLOCK, STREAM, STORED]) {
      const src = source(url);
      assert(!/\{@html\s/.test(src), "stored text must never become markup");
    }
    const call = source(BODY);
    assert(
      !call.includes("href"),
      "stored text must not become a link, whatever it says",
    );
    // The rest of a prefix is a control: the block that holds the text asks
    // the page for it, and names the range the rest will come in.
    const detail = source(DETAIL);
    assert(
      detail.includes("<StoredText"),
      "a message's stored text must offer its rest",
    );
    const stored = source(STORED);
    assert(
      stored.includes("manage_turns_body_range({"),
      "the block names the range its rest will come in",
    );
    for (const label of [
      "manage_turns_body_range({",
      "manage_turns_body_more()",
      "manage_turns_body_empty()",
      "manage_turns_body_reclaimed({",
    ]) {
      assert(call.includes(label), `a call block must render ${label}`);
    }
    // The read itself stays on the page, which is where the belonging check
    // lives.
    const pageSource = source(PAGE);
    assert(
      pageSource.includes(
        "backend.callBody(id, turnId, callIndex, { part, offset })",
      ),
      "a body range must be read through the backend's callBody",
    );
    assert(
      pageSource.includes(
        "backend.messageBody(id, turnId, messageIndex, { part, offset })",
      ),
      "a message body range must be read through the backend's messageBody",
    );
  },
);

Deno.test(
  "a stored block is keyed by its slot, so another turn starts over",
  { permissions: { read: [DETAIL, CALL, BODY, STORED] } },
  () => {
    // Svelte reuses a component by position: an unkeyed block would keep the
    // ranges of the text it no longer shows, and the read for the new text
    // would never start. The key is the block's slot: turn, index, and which
    // part, so a different text lands in a fresh block.

    const blocks: [URL, RegExp][] = [
      [DETAIL, /^bodyKey\(turn\.turn_id, slot\.index, "(text|reasoning)"\)$/],
      [CALL, /^bodyKey\(turnId, call\.call_index, "error"\)$/],
      [BODY, /^bodyKey\(turnId, call\.call_index, "args"\)$/],
    ];
    for (const [url, identity] of blocks) {
      const src = source(url);
      const keyed = [
        ...src.matchAll(
          /\{#key ([^}]*)\}\n\s*<StoredText\b[\s\S]*?\n\s*\{\/key\}/g,
        ),
      ];
      assertEquals(
        keyed.length,
        count(src, "<StoredText"),
        `${url.pathname}: every stored block is keyed`,
      );
      for (const match of keyed) {
        assert(
          identity.test(match[1].trim()),
          `${url.pathname}: the key is the text's identity, not its index`,
        );
      }
    }
    // A fresh block draws the text it holds, and under it the control that
    // reads the rest: the text is on screen before any click.
    const stored = source(STORED);
    assert(
      stored.indexOf("{#if text !== null}") < stored.indexOf("{#if more}"),
      "a fresh block draws its text before its control",
    );
  },
);

Deno.test(
  "the list keeps no-log, empty and failed apart",
  { permissions: { read: [LIST, PAGE] } },
  () => {
    const list = source(LIST);
    assert(
      list.includes('fire === "no-log"') &&
        list.includes("manage_turns_no_log()"),
      "a list 404 must render the no-log line",
    );
    assert(
      list.includes("manage_turns_empty()"),
      "an empty page must render the empty line",
    );
    assert(
      list.includes('fire === "failed"') &&
        list.includes("manage_turns_load_failed()"),
      "a failed read must render the failure line",
    );
    const page = source(PAGE);
    assert(
      page.includes('errorState("list", errorStatus(e))'),
      "the list failure must be classified by the decision layer",
    );
    assert(
      page.includes('errorState("detail", errorStatus(e))'),
      "a vanished turn must be classified, not matched against server prose",
    );
    assert(
      page.includes("displayError("),
      "a failed read shows the server's own message, never a fixed sentence",
    );
  },
);

Deno.test(
  "reasoning folds, and stays out of the DOM until opened",
  { permissions: { read: [DETAIL] } },
  () => {
    const src = source(DETAIL);
    assert(
      src.includes("manage_turns_reasoning_show()") &&
        src.includes("manage_turns_reasoning_hide()"),
      "the reasoning fold needs both labels",
    );
    assert(
      src.includes("manage_turns_reasoning_unavailable()"),
      "a flag without text must say so instead of folding nothing",
    );
    const open = src.indexOf(
      "{@const open = reasoningOpen(turn.turn_id, slot.index)}",
    );
    const gate = src.indexOf("{#if showsThought}");
    const body = src.indexOf("text={thought.text}");
    assert(open > 0, "the fold reads the display state once per message");
    assert(
      gate > open,
      "the reasoning text must sit behind the gate that opens the box",
    );
    assert(body > gate, "the reasoning text must not render while folded");
  },
);

Deno.test(
  "older turns are paged with the cursor the page was given",
  { permissions: { read: [PAGE, LIST] } },
  () => {
    const page = source(PAGE);
    assert(
      page.includes("limit: TURNS_PAGE_LIMIT"),
      "a page asks for the default row count",
    );
    assert(
      page.includes("before: cursor"),
      "an older page is asked for by cursor",
    );
    assert(
      page.includes("canLoadMore(page, loadingMore)"),
      "the paging control must follow the decision layer's verdict",
    );
    assert(
      source(LIST).includes("manage_turns_load_more()"),
      "the paging control must render its label",
    );
  },
);

Deno.test(
  "the agent detail links to the turn page instead of mounting the list",
  { permissions: { read: [AGENT] } },
  () => {
    const src = source(AGENT);
    assert(
      !src.includes("TurnHistoryCard") && !src.includes("TurnList"),
      "the list moved to its own page; mounting it here would fetch turns twice",
    );
    assert(
      src.includes("agentHistoryPath(basePath, id)"),
      "the entry link must come from the base-path builder (online and offline trees)",
    );
  },
);

Deno.test(
  "the turn page mounts the list and the detail with its own reads",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    assert(
      src.includes("<TurnList") && src.includes("<TurnDetail"),
      "the page must mount both columns' components",
    );
    assert(
      src.includes("onReadBody={readBody}") &&
        src.includes("onMoreBody={moreBody}"),
      "call texts are read through the page's callbacks",
    );
  },
);

Deno.test(
  "an opened turn reads the first results without being asked",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    // After the staleness check, so a turn the reader already left opens
    // nothing, and before the catch, so a failed detail read reads no body.
    const landed = between(src, "detail = read;", "} catch (e) {");
    assert(
      landed.includes("untouchedResults(") &&
        landed.includes('readBody(turnId, callIndex, "result", 0)'),
      "the first results must come back with the turn, not on a click",
    );
    // A turn's slots must be empty before the automatic reads open, or a
    // block from the turn before would swallow them. Both the clear and the
    // open live in `openTurn`: the page clears again when the agent changes,
    // and that other clear must not stand in for this one.
    const openTurn = between(src, "async function openTurn", "} catch (e) {");
    const cleared = openTurn.indexOf("texts.clear()");
    assert(
      cleared !== -1 && cleared < openTurn.indexOf("untouchedResults("),
      "the page clears the turn's blocks before it opens the new ones",
    );
  },
);

Deno.test(
  "a screen too narrow for two columns shows one at a time",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    assert(
      src.includes("class:hidden={!showList}") &&
        src.includes("class:hidden={!showDetail}"),
      "each column hides by state, not by a second viewport branch in markup",
    );
    assert(
      src.includes("lg:hidden"),
      "the back control belongs to the stacked layout only",
    );
  },
);

Deno.test(
  "each column scrolls on its own, and a pick starts at the top",
  { permissions: { read: [PAGE, LIST] } },
  () => {
    const page = source(PAGE);
    assert(
      page.includes(
        'class="flex h-full flex-col overflow-y-auto lg:overflow-hidden"',
      ),
      "the stacked layout keeps the page as its one scroller",
    );
    assert(
      page.includes(
        'class="space-y-2 lg:min-h-0 lg:overflow-y-auto lg:overscroll-contain"',
      ) &&
        page.includes(
          'class="min-w-0 space-y-2 lg:min-h-0 lg:overflow-y-auto lg:overscroll-contain"',
        ),
      "each column carries its own scrollbar: one class string per column",
    );
    assert(
      page.includes("bind:this={detailPane}") &&
        page.includes("resetReaderTop();"),
      "a wide screen scrolls each column, and a pick sends the turn to its top",
    );
    assert(
      page.includes("row.focus();") &&
        page.includes('row.scrollIntoView({ block: "nearest" })'),
      "going back to the list leaves the focus on the row it came from",
    );
    const list = source(LIST);
    assert(
      list.includes("onkeydown={onKey}") &&
        list.includes("data-turn={row.turn_id}"),
      "arrow keys walk the rows that carry a turn id",
    );
  },
);

Deno.test(
  "below 64rem the page root stays the only scroller",
  { permissions: { read: [PAGE, BREAK] } },
  () => {
    const forks = source(BREAK);
    assert(
      forks.includes('export const lgQuery = "(min-width: 64rem)"'),
      "the two-column fork sits at 64rem, above the shell's 48rem nav fork",
    );
    const page = source(PAGE);
    assert(
      page.includes('import { lgQuery } from "../../lib/shell/breakpoint.ts"'),
      "the page reads the shared fork instead of keeping its own copy",
    );
    assert(
      !page.includes('const lgQuery = "'),
      "a second copy of the width would be a second truth",
    );
  },
);

Deno.test(
  "every read asks whether the answer still belongs before it writes",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    const swap = between(
      src,
      "if (loadedFor === id) return;",
      "void loadFirst();",
    );
    assert(
      swap.includes("readEpoch += 1;"),
      "a swap must start a new era, or the old agent's answers still land",
    );
    for (const flag of ["loading = false;", "loadingMore = false;"]) {
      assert(
        swap.includes(flag),
        `a swap must clear ${flag}: a dropped answer never reaches its finally`,
      );
    }
    const ask = "stillCurrent(";
    const paths: [string, string, string][] = [
      [
        "async function loadFirst()",
        "async function loadMore()",
        "rows = mergePages([], first)",
      ],
      [
        "async function loadMore()",
        "async function openTurn(",
        "rows = mergePages(rows, next)",
      ],
      ["async function openTurn(", "function readBody(", "detail = read;"],
    ];
    for (const [from, to, write] of paths) {
      const path = between(src, from, to);
      assert(
        count(path, ask) >= 3,
        `${from} lost a guard: move this pin with the guard`,
      );
      assert(
        path.indexOf(write) > path.indexOf(ask),
        `${from} must ask before ${write}`,
      );
    }
    assertEquals(
      count(src, ask),
      9,
      "three reads, three asks each: a new guard means this pin moves too",
    );
    assertEquals(
      count(src, "if (loading || loadingMore) return;"),
      2,
      "both list entries must turn away while a read is in flight",
    );
  },
);
Deno.test(
  "the body-read bookkeeping is delegated to the stored-text owner",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    for (const call of ["texts.begin(", "texts.settle("]) {
      assert(src.includes(call), `a body read must go through ${call}`);
    }
    // The stamp, the comparison and the placeholder all live in one module
    // now: a page that wrote the entry itself is how a read that forgot to
    // stamp its block went unnoticed, so the page holds none of the three.
    for (const gone of ["startRead(", "bodyStillMine(", "finishRead("]) {
      assert(!src.includes(gone), `the page must not call ${gone} itself`);
    }
    assert(
      !src.includes(".token ==="),
      "the page must not compare read tokens itself",
    );
  },
);

Deno.test(
  "a call's shape comes from the data, never from its name",
  { permissions: { read: [BODY, CALL] } },
  () => {
    const src = source(BODY);
    assert(
      src.includes("callShape(") && src.includes("terminalOutput("),
      "the shape and its streams must come from the decision layer",
    );
    assert(
      !/call\.name\s*===/.test(src) && !src.includes("SUMMARY_FIELDS"),
      "a name comparison here would be a guess the data already answers",
    );
    for (const name of [
      "bash_exec",
      "bash_background_read",
      "context_pin",
      "context_evict",
      "context_unpin",
    ]) {
      assert(
        !src.includes(name),
        `a tool name in the view would be a name comparison by another route`,
      );
    }
    const shell = source(CALL);
    assert(
      shell.includes("<CallBody") && !shell.includes("callShape("),
      "the call header must delegate the body to its one component",
    );
    assert(
      src.includes("manage_turns_text_unsafe_number()") &&
        src.includes("manage_turns_text_too_large()"),
      "a text that cannot be laid out must say so instead of showing nothing",
    );
    assert(
      src.includes("{#if command !== null}"),
      "a command line appears only where the arguments carried one",
    );
    assert(
      src.includes("hasStructured"),
      "the view switch appears only when a second view exists",
    );
  },
);

Deno.test(
  "the tree folds through real buttons and never navigates",
  { permissions: { read: [TREE] } },
  () => {
    const src = source(TREE);
    assertEquals(
      count(src, "aria-expanded={open}"),
      1,
      "a container opens through a control that reports whether it is open",
    );
    assert(
      src.includes("<button") && !src.includes("href"),
      "folding is a real button, and a node is not a place to navigate to",
    );
  },
);

Deno.test(
  "a block's controls act on the text the block draws",
  { permissions: { read: [BODY] } },
  () => {
    const src = source(BODY);
    const arm = between(src, "{:else if atEnd}", "{:else}");
    assert(
      arm.includes("<BlockText"),
      "an arm that draws stored text hands it to the block that owns the control",
    );
    assert(
      !arm.includes("{text}</pre>"),
      "an arm must not draw text the fold control leaves alone",
    );
    assert(
      src.includes('{#if text !== "" && atEnd}'),
      "the copy control appears only when every byte of the text is here",
    );
    // The arguments' copy control holds the whole string or is not drawn: an
    // uncut row has it at hand, a prefix row has to be read to the end.
    const whole = between(src, "{#if !args.more}", "{:else if argsBody");
    assert(
      whole.includes("getText={() => call.args}"),
      "an uncut row's copy hands over the stored string itself",
    );
    const reached = between(
      src,
      "{:else if argsBody !== undefined && argsBody.atEnd}",
      "{/if}",
    );
    // What the copy hands over is pinned by content in turnView_test (the
    // prefix plus the range, never the range alone); here the question is only
    // whether the control goes through that one spelling.
    assert(
      reached.includes("{@const text = wholeText(args.text, argsBody)}") &&
        reached.includes("getText={() => text}"),
      "a prefix row's copy appears once the read reaches the end, and hands over the whole argument",
    );
    assertEquals(
      count(src, "getText={() => body?.text"),
      1,
      "one copy control reads the stored text",
    );
  },
);

// The rule that decides which turn is on show lives in turnView.ts and is
// tested in turnView_test. Whether the page hands its state changes to
// that decision is not visible from source: there is no mount-level
// harness here, so a deleted call site is caught by reading and in a
// browser, not by running. The page's two calls to that decision, the rows
// landing and the media handler, are not pinned either.
Deno.test(
  "a landing list reads as loading, and an unreadable list is not called empty",
  { permissions: { read: [PAGE] } },
  () => {
    const page = source(PAGE);
    assert(
      page.includes(
        "loading={detailLoading || (loading && rows.length === 0)}",
      ),
      "a list still arriving reads as loading, not as an empty agent",
    );
    const empty = between(page, "empty={", "}");
    assert(
      empty.includes("listFace === null"),
      "a list this page cannot read is not also reported as having no turns",
    );
  },
);

Deno.test(
  "the page asks for reads and never assembles a block entry",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    for (const gone of ["bodies =", "bodies[", "bodyToken", "[key]: started"]) {
      assert(
        !src.includes(gone),
        `the page holds no ${gone}: the owner of the stored text writes it`,
      );
    }
    const read = between(src, "async function readRange(", "</script>");
    assertEquals(
      count(read, "texts.begin("),
      1,
      "one place opens a read, and it is the owner",
    );
    assertEquals(
      count(read, "texts.settle("),
      2,
      "both outcomes settle through the owner",
    );
  },
);

Deno.test(
  "a stored text is drawn once, in one box",
  { permissions: { read: [BLOCK, BODY, STREAM] } },
  () => {
    const block = source(BLOCK);
    assertEquals(count(block, ">{text}</pre"), 1, "one block draws one box");
    const markup = block.slice(block.indexOf("</script>"));
    const openTag = markup.slice(
      markup.indexOf("<pre"),
      markup.indexOf(">", markup.indexOf("<pre")),
    );
    // The base box is no well: moving the height and the scroll onto it
    // would put a message back in a 24rem window without any count moving.
    assert(
      !openTag.includes("overflow") && !openTag.includes("max-h"),
      "the base box neither caps its height nor scrolls",
    );
    assert(
      !block.includes("manage_turns_show_all") &&
        !block.includes("manage_turns_show_less") &&
        !block.includes("atEnd"),
      "a line count is no answer to whether the text is all here: no fold",
    );
    for (const url of [BODY, STREAM]) {
      assert(
        !source(url).includes("manage_turns_show_all"),
        "no other component offers a fold either",
      );
    }
    assertEquals(
      count(source(BODY), "<pre"),
      0,
      "the call body draws every text through the one block component",
    );
  },
);

Deno.test(
  "one dispatch, and the registry decides it",
  { permissions: { read: [BODY, REGISTRY] } },
  () => {
    const body = source(BODY);
    assertEquals(
      count(body, "pickRegisteredView("),
      1,
      "one place asks the registry",
    );
    assertEquals(
      body.match(/<View[^A-Za-z]/g)?.length ?? 0,
      1,
      "one place renders a dedicated view",
    );
    const bodyTemplate = body.slice(body.indexOf("</script>"));
    const pendingArm = between(
      bodyTemplate,
      'pick.kind === "pending"',
      'pick.kind === "dedicated"',
    );
    assert(
      pendingArm.includes('tone="info"') &&
        pendingArm.includes("manage_turns_pending_no_id()") &&
        pendingArm.includes(
          "manage_turns_pending_line({ id: pick.pending.id })",
        ),
      "a deferred call waits, and says which approval it waits on",
    );
    assert(
      pendingArm.includes("mono={pick.pending.id !== null}"),
      "the id the sentence prints is the one to copy, so it sets in mono",
    );
    assert(
      pendingArm.includes("{manage_turns_pending_hint()}"),
      "the waiting call says what turns the wait into a run",
    );
    assert(
      !pendingArm.includes("pick.pending.nextSteps"),
      "the daemon's own next step is not echoed under the sentence",
    );
    assert(
      pendingArm.includes("pick.pending.id !== null"),
      "the hint that speaks of an id waits for a record that carries one",
    );
    assertEquals(
      count(bodyTemplate, "manage_turns_fallback_shown"),
      1,
      "one line says a registered tool fell back",
    );
    const fallback = between(
      body,
      'pick.kind === "failed"',
      "manage_turns_fallback_shown",
    );
    assert(
      fallback.includes("{:else}") &&
        fallback.includes("{#if pick.registered}"),
      "the fallback line belongs to the generic path, under its own condition",
    );
    // Placement only: this says where the fallback wording sits, not that the
    // arm can be reached. Reachability is parseEnvelope's unit test.
    const failure = between(body, 'pick.kind === "failed"', "{:else}");
    assert(
      failure.includes("manage_turns_failed_line(") &&
        failure.includes("pick.error ?? manage_turns_error_unstated()"),
      "the failure arm states its failure and falls back to this wording",
    );
    for (const literal of ['"command"', '"exit_code"']) {
      assert(
        !body.includes(literal),
        "the call body must not probe a tool's fields by name",
      );
    }
    const registry = source(REGISTRY);
    assertEquals(count(registry, ": view("), 6, "one line per registered tool");
    const wired = [
      "bash_exec: view(isBashExecResult",
      "break: view(isBreakResult",
      "bash_background_read: view(isBgReadResult",
      "context_evict: view(isEvictResult",
      "context_pin: view(isContextPinResult",
      "context_unpin: view(isContextUnpinResult",
    ];
    for (const line of wired) {
      assert(
        registry.includes(line),
        "each name is wired to its own validator, not to whichever one is near",
      );
    }
  },
);

Deno.test(
  "the terminal view draws its output through the one stream block",
  { permissions: { read: [TERMINAL_VIEW, STREAM, BODY] } },
  () => {
    const view = source(TERMINAL_VIEW);
    assertEquals(
      count(view, "<OutputStream"),
      1,
      "one terminal output, one implementation",
    );
    assertEquals(
      count(view, "<pre"),
      0,
      "the view places no text box of its own",
    );
    assert(
      view.includes("terminalCommand("),
      "the command comes from the one reader",
    );
    assert(!view.includes('"command"'), "no field is probed by name here");
    const out = source(STREAM);
    assert(
      out.includes("pickStream("),
      "the stream is chosen by the shared rule",
    );
    assert(
      out.includes("output.exitCode"),
      "the failure edge is read where the code is drawn",
    );
    assertEquals(
      count(source(BODY), "<OutputStream"),
      1,
      "the shape chain draws terminal output with the same block",
    );
  },
);

Deno.test(
  "a view is only handed a payload its validator accepted",
  { permissions: { read: [BODY, TERMINAL_VIEW, BREAK_VIEW] } },
  () => {
    const body = source(BODY);
    const dedicated = between(
      body,
      'pick.kind === "dedicated"',
      'pick.kind === "failed"',
    );
    assert(
      dedicated.includes("payload={pick.payload}"),
      "the payload comes from the pick, not from a re-read of the text",
    );
    for (const url of [TERMINAL_VIEW, BREAK_VIEW]) {
      const view = source(url);
      assert(
        view.includes("CallViewProps<") && !view.includes("| undefined"),
        "a view takes props whose payload is required: the failed leg never reaches it",
      );
    }
  },
);

Deno.test(
  "the two views say what happened, not which fields said it",
  { permissions: { read: [TERMINAL_VIEW, BREAK_VIEW] } },
  () => {
    const terminal = source(TERMINAL_VIEW);
    const brk = source(BREAK_VIEW);
    for (const view of [terminal, brk]) {
      for (const wire of [
        '"exit_code"',
        '"cwd"',
        '"task_id"',
        '"timed_out"',
        '"parked"',
      ]) {
        assert(
          !view.includes(wire),
          "a view names no wire field to the reader",
        );
      }
    }
    assertEquals(count(brk, "<p"), 0, "break draws no text box of its own");
    assertEquals(count(brk, "<CallNarration"), 1, "break says its one line");
    assertEquals(
      count(terminal, "<CallNarration"),
      1,
      "terminal says its line",
    );
    assert(
      terminal.indexOf("<CallNarration") < terminal.indexOf("<span class"),
      "the narration comes before the command it describes",
    );
    assert(
      !terminal.includes("text-error-500"),
      "the outcome line paints no text: the dot carries the state",
    );
  },
);

Deno.test(
  "a failed read leaves a way to read again",
  { permissions: { read: [BODY] } },
  () => {
    const body = source(BODY);
    const arm = between(
      body,
      "{:else if body.error}",
      "{:else if body.unsupported !== null}",
    );
    assert(
      arm.includes("manage_turns_refresh"),
      "a failed read offers the same read again, in the words the page uses",
    );
    assert(
      arm.includes('onclick={() => onRead("result", 0)}'),
      "and it asks for the same read the page already owns",
    );
  },
);

Deno.test(
  "the two new views say what happened, and place no text box of their own",
  { permissions: { read: [BG_READ_VIEW, EVICT_VIEW] } },
  () => {
    const bg = source(BG_READ_VIEW);
    const evict = source(EVICT_VIEW);
    for (const view of [bg, evict]) {
      for (const wire of [
        '"state"',
        '"stalled"',
        '"bytes"',
        '"evicted"',
        '"kept_turns"',
        '"freed_tokens"',
      ]) {
        assert(
          !view.includes(wire),
          "a view names no wire field to the reader",
        );
      }
      assertEquals(view.includes("<pre"), false, "no text box of its own");
      assertEquals(
        view.includes("fetch(") || view.includes("client."),
        false,
        "a view reads nothing itself",
      );
      assertEquals(
        count(view, "<CallNarration"),
        1,
        "one line says how it went",
      );
    }
    assertEquals(
      count(bg, "<BlockText"),
      1,
      "the tail is drawn by the one text block",
    );
    // Each assertion below names a branch: delete the {#if} and the line
    // never renders, so this test goes red.
    assert(
      count(bg, "{#if facts.stalled}") === 1 &&
        bg.includes("manage_turns_bg_stalled()") &&
        bg.includes("text-error-900-100"),
      "a stalled task says so, in the one alarm this page has",
    );
    assert(
      bg.includes("{#if facts.folded}") &&
        bg.includes("manage_turns_bg_tail()"),
      "a folded tail says the start is not in this read",
    );
    assert(
      evict.includes("{#if keptLine !== null}") &&
        count(evict, "manage_turns_evict_kept(caps)") === 1,
      "the keep window is drawn when the record states one",
    );
    assert(
      count(evict, "{#if facts.capZero}") === 1 &&
        count(evict, "{#if facts.degraded !== null}") === 1,
      "the two lines that explain a surprising count stay",
    );
    assert(
      count(bg, "bgReadTone(") === 1 && count(evict, "evictTone(") === 1,
      "the dot's tone is one function the view calls, not a switch it keeps",
    );
  },
);

Deno.test(
  "the pin view names no wire field, and its chips follow the projected facts",
  { permissions: { read: [CONTEXT_OP_VIEW] } },
  () => {
    const view = source(CONTEXT_OP_VIEW);
    for (const wire of ['"unpinned"', '"pinned_labels"']) {
      assert(!view.includes(wire), "a view names no wire field to the reader");
    }
    assertEquals(view.includes("<pre"), false, "no text box of its own");
    assertEquals(
      view.includes("fetch(") || view.includes("client."),
      false,
      "a view reads nothing itself",
    );
    assertEquals(
      count(view, "<CallNarration"),
      1,
      "one line tells what the call did",
    );
    // One body for two names: the verb chooses the sentence, nothing else.
    assert(
      view.includes('facts.verb === "pinned"') &&
        count(view, "contextOpFacts(") === 1,
      "the reply is projected once, and the verb chooses the sentence",
    );
    const chips = between(view, "{#if facts.labels.length > 0}", "</div>");
    assert(
      chips.includes("{#each facts.labels as label}") &&
        !chips.includes("{#each payload"),
      "the chips are the projected labels, never a second read of the record",
    );
    assert(
      !view.includes(".slice("),
      "a set too big to show whole draws no chips, and nothing is sliced",
    );
  },
);

Deno.test(
  "a read that ends early says which of the two reasons applies",
  { permissions: { read: [STORED] } },
  () => {
    const src = source(STORED);
    const OPN = "{:else if !body.atEnd}";
    // A finished read draws neither control: no next range and no apology.
    // The guard is what keeps "the end arrived" from reading as "it stopped
    // short of the end".
    const moreArm = between(src, "{#if body.more !== null}", OPN);
    assert(
      moreArm.includes("manage_turns_body_more()") &&
        moreArm.includes("onclick={readMore}"),
      "a face that names the next range offers exactly that range",
    );
    const stopped = src.indexOf("manage_turns_body_stopped(");
    const stoppedOpener = src.slice(
      src.lastIndexOf("{:else", stopped),
      stopped,
    );
    assert(
      stoppedOpener.includes(OPN),
      "the apology is for a range that stopped short, never for the end",
    );
    assert(
      !src.includes("manage_turns_body_end"),
      "the end of the text is not announced twice",
    );
    const stoppedArm = between(src, "manage_turns_body_stopped(", "{/if}");
    assert(
      stoppedArm.includes("onclick={readRest}") &&
        stoppedArm.includes("manage_turns_refresh()"),
      "a range that stops short of the end offers the read again",
    );
    assert(
      src.includes("{#if body.reclaimed > 0}") &&
        src.includes("manage_turns_body_reclaimed({ count: body.reclaimed })"),
      "reclaimed stream paths are named where the text sits",
    );
  },
);

Deno.test(
  "a range read for another message never reaches the text it would sit in",
  { permissions: { read: [PAGE, DETAIL] } },
  () => {
    const page = source(PAGE);
    // The page, not the component, decides: a range that belongs to another
    // message is refused before any text is joined, and the reader is told.
    const guard = between(
      page,
      "if (expectRole !== undefined && read.role !== expectRole) {",
      "}",
    );
    assert(
      guard.includes("manage_turns_body_wrong_message()"),
      "a range from another message is refused, and says why",
    );
    const detail = source(DETAIL);
    const role =
      count(detail, "slot.message.role)") + count(detail, "slot.message.role,");
    assertEquals(
      role,
      4,
      "every read the detail asks for carries the role it was drawn from",
    );
  },
);

Deno.test(
  "a count reads as a number in the reader's own grouping",
  { permissions: { read: [DETAIL, STORED, BODY] } },
  () => {
    assert(
      !source(DETAIL).includes("manage_turns_message_chars"),
      "a message states its role, not how many characters it kept",
    );
    for (const url of [STORED, BODY]) {
      const src = source(url);
      for (const field of ["from", "to", "total"]) {
        assert(
          src.includes(`body.${field}.toLocaleString()`),
          `${url.pathname}: a range's ${field} groups its digits`,
        );
      }
    }
  },
);

Deno.test(
  "a stored text scrolls in its own box, and a failed read keeps a retry",
  { permissions: { read: [STORED] } },
  () => {
    const src = source(STORED);
    // The box is one component's job: a stored text becomes a `<pre>` in one
    // place, so a message and a tool record read the same in it.
    assert(
      src.includes('from "./BlockText.svelte"') &&
        count(src, "<BlockText ") === 6,
      "every text a block draws goes through the one stored-text box",
    );
    assert(!src.includes("{@html"), "the box draws text, never markup");
    // A failed read says so and offers a control that reads again: the error
    // must not leave the reader with a dead block.
    const retry = between(
      src,
      "{:else if body.error !== null}",
      "{:else if body.unsupported",
    );
    assert(
      retry.includes("{body.error}") &&
        retry.includes("manage_turns_refresh()") &&
        retry.includes("preset-outlined-surface-500"),
      "the error is drawn with a button the reader can see, not a grey word",
    );
    assert(
      retry.indexOf("<BlockText") < retry.indexOf("{body.error}"),
      "a failed read keeps the prefix the reader already has",
    );
  },
);

Deno.test(
  "a first read asks from where the drawn text ends",
  { permissions: { read: [STORED] } },
  () => {
    const src = source(STORED);
    assert(
      src.includes("const restFrom = $derived(prefixBytes(text))") &&
        src.includes("onRead(restFrom)"),
      "the rest is asked for from the byte the prefix ends at, not from zero",
    );
    assert(
      src.includes("wholeText(text, body)"),
      "the answer is drawn after the prefix, so no text is shown twice",
    );
  },
);

Deno.test(
  "the stored text box tells machine output from prose",
  { permissions: { read: [BLOCK, DETAIL, STORED] } },
  () => {
    const block = source(BLOCK);
    assert(
      block.includes('kind?: "machine" | "prose"'),
      "the box takes the one fact it cannot read off the text",
    );
    assert(
      block.includes('kind === "machine"') &&
        block.includes("bg-surface-50-950"),
      "machine output reads as a terminal",
    );
    // The terminal dress belongs to one branch: a prose block that also took
    // the monospace face, or a well to scroll in, would hide what the kind
    // drew.
    assertEquals(
      count(block, "font-mono"),
      1,
      "the monospace face is the machine branch alone",
    );

    const well = block
      .split("\n")
      .filter((line) => line.includes("overflow-auto"));
    assertEquals(well.length, 1, "one line brings the well");
    assert(
      well[0].includes("font-mono"),
      "the well belongs to the machine face",
    );
    assertEquals(
      count(source(DETAIL), 'kind="prose"'),
      2,
      "a message and its reasoning are prose, not terminal output",
    );
    assertEquals(
      count(source(STORED), "{kind}"),
      6,
      "the block passes the kind to every text it draws",
    );
  },
);

Deno.test(
  "a prefix is said once, by the control that reads the rest",
  { permissions: { read: [BODY] } },
  () => {
    const src = source(BODY);
    const row = between(src, "{manage_turns_args_label()}", "</div>");
    assertEquals(
      count(row, "manage_turns_args_"),
      1,
      "the title row names the half, once",
    );
    assert(
      row.includes("CopyButton"),
      "the title row keeps the copy control beside the half it names",
    );
    assert(
      !src.includes("manage_turns_args_cut_note") &&
        !src.includes("manage_turns_args_stored_cut"),
      "the prefix is carried by the read control and the range, nowhere else",
    );
    const cut = between(src, "{:else if args.more}", "{:else}");
    assert(
      cut.includes("<StoredText") && cut.includes("more"),
      "the prefix is drawn by the one block that is told to read its rest",
    );
  },
);

Deno.test(
  "a count of one reads as one",
  { permissions: { read: [CALL, LIST] } },
  () => {
    const call = source(CALL);
    assert(
      call.includes("size.lines === 1") && call.includes("size.bytes === 1"),
      "each count picks its own singular",
    );
    for (const key of [
      "manage_turns_stream_line_one",
      "manage_turns_stream_line_other",
      "manage_turns_stream_byte_one",
      "manage_turns_stream_byte_other",
    ]) {
      assert(call.includes(key), `a stream's size names ${key}`);
    }
    const list = source(LIST);
    assert(
      list.includes("notice.count === 1") &&
        list.includes("manage_turns_bad_lines_one"),
      "one unread line is reported as one line",
    );
  },
);

Deno.test(
  "a cleared selection goes through the one place that owns its fields",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    const clear = between(src, "function clearSelection(): void {", "}");
    for (const field of [
      "selectedId = null;",
      "detail = null;",
      "autoPicked = false;",
      "detailLoading = false;",
      'detailFire = "none";',
      "detailError = null;",
    ]) {
      assert(clear.includes(field), `clearSelection must reset ${field}`);
    }
    assertEquals(
      src.split("selectedId = null;").length - 1,
      1,
      "one field list: a second place that writes this field is what the count finds",
    );
    const swap = between(
      src,
      "if (loadedFor === id) return;",
      "void loadFirst();",
    );
    assert(
      swap.includes("clearSelection();") &&
        !swap.includes("detailLoading = false;"),
      "swapping agents clears through the owner, not a second field list",
    );
    const back = between(src, "async function backToRow", "await tick();");
    assert(
      back.includes("clearSelection();") &&
        !back.includes("selectedId = null;"),
      "backing out of a turn clears through the owner too",
    );
    const drop = between(src, "function applyShownTurn", "}");
    assert(
      drop.includes("clearSelection();") &&
        !drop.includes("selectedId = null;"),
      "dropping a layout pick clears through the owner as well",
    );
  },
);

Deno.test(
  "a list row names what the turn was, and counts only tool calls",
  { permissions: { read: [LIST] } },
  () => {
    const src = source(LIST);
    assert(
      src.includes("turnShape(row)"),
      "the shape comes from the decision layer, not from the template",
    );
    const tools = between(
      src,
      `{#if shape === "tools"}`,
      `{:else if shape === "user"}`,
    );
    assert(
      tools.includes("manage_turns_calls_label()") &&
        tools.includes("row.calls.length"),
      "the call count sits inside the tool branch, not on every row",
    );
    const user = between(
      src,
      `{:else if shape === "user"}`,
      `{:else if shape === "assistant"}`,
    );
    assert(
      user.includes("manage_turns_shape_user()") &&
        !user.includes("calls.length"),
      "a user turn names itself and carries no call count",
    );
    const reply = between(src, `{:else if shape === "assistant"}`, "{/if}");
    assert(
      reply.includes("manage_turns_shape_assistant()"),
      "a reply names itself",
    );
  },
);

Deno.test(
  "the detail pane draws the messages worth drawing, keyed on their index",
  { permissions: { read: [DETAIL] } },
  () => {
    const src = source(DETAIL);
    assert(
      src.includes("{@const shown = visibleMessages(turn)}"),
      "the pane hands the whole turn over: which messages are worth drawing",
    );
    assert(
      src.includes("{#each shown as slot (slot.index)}") &&
        !src.includes("as message, index"),
      "a filtered message keeps the index its body reads are keyed on",
    );
    assert(
      src.includes("{#if shown.length > 0}") &&
        !src.includes("{#if turn.messages.length > 0}"),
      "the message block appears with its messages, never as a bare heading",
    );
    assert(
      !src.includes("{#each turn.messages"),
      "the pane never falls back to drawing every message",
    );
    // The body read is keyed on the drawn slot's own index: a position in the
    // filtered list would read another message's text.
    // Four reads, each naming the slot's own index: an off-by-one, a position
    // in the filtered list, or another message's role all change this list.
    const squeeze = (text: string) =>
      text.replace(/\s+/g, "").replace(/,\)$/, ")");
    const reads = [...src.matchAll(/on(?:Read|More)Message\([^)]*\)/g)].map(
      (m) => squeeze(m[0]),
    );
    assertEquals(
      reads,
      [
        'onReadMessage(turn.turn_id,slot.index,"text",slot.message.role,offset)',
        'onMoreMessage(turn.turn_id,slot.index,"text",offset,slot.message.role)',
        'onReadMessage(turn.turn_id,slot.index,"reasoning",slot.message.role,offset)',
        'onMoreMessage(turn.turn_id,slot.index,"reasoning",offset,slot.message.role)',
      ].map(squeeze),
      "every body read asks for the slot's own index",
    );
    assertEquals(
      count(src, "bodyKey(turn.turn_id, slot.index,"),
      4,
      "both keys and both stored lookups, text and reasoning, use that index",
    );
    assert(
      !src.includes("turn.turn_id, index"),
      "no body read asks the face for a filtered position",
    );
  },
);

Deno.test(
  "a message with no text draws no box until its reasoning is opened",
  { permissions: { read: [DETAIL] } },
  () => {
    const src = source(DETAIL);
    // The block opens for text or for a thought the reader has unfolded: a
    // reply that only reasoned keeps its role line and opens the block once
    // the reader asks for the thought.
    const block = between(
      src,
      "{#if carries || showsThought}",
      "{manage_turns_reasoning_unavailable()}",
    );
    assert(
      block.includes("{#if showsThought}") &&
        block.includes("text={thought.text}"),
      "the reasoning text sits inside that block, behind the same gate that opens the box",
    );
    const roleLine = between(
      src,
      'class="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs opacity-70"',
      "</div>",
    );
    assert(
      roleLine.includes("manage_turns_reasoning_show()") &&
        roleLine.includes("manage_turns_reasoning_hide()"),
      "the reasoning control sits on the role line",
    );
    assert(
      src.includes("carriesText(view.text, view.more)"),
      "a blank text draws nothing, and a blank prefix is still a prefix",
    );
  },
);

Deno.test(
  "the terminal line leaves the outcome to the call's own card",
  { permissions: { read: [TERMINAL_VIEW, CALL] } },
  () => {
    const terminal = source(TERMINAL_VIEW);
    const call = source(CALL);
    assert(
      terminal.includes("manage_turns_terminal_ran") &&
        !terminal.includes("manage_turns_terminal_succeeded") &&
        !terminal.includes("manage_turns_terminal_failed"),
      "the terminal line says where the command ran, not how it ended",
    );
    assert(
      call.includes("manage_turns_call_exit") &&
        call.includes("manage_turns_call_failed()"),
      "the card says the outcome and the exit code the line leaves out",
    );
  },
);

Deno.test(
  "one call reads as who, what went in, and what came back",
  { permissions: { read: [BODY] } },
  () => {
    const body = source(BODY);
    assert(
      body.indexOf("{manage_turns_args_label()}") <
        body.indexOf("{manage_turns_result_label()}"),
      "the arguments half is titled before the result half",
    );
    const result = between(
      body,
      "{manage_turns_result_label()}",
      "{#if body === undefined}",
    );
    assert(
      result.includes("CopyButton"),
      "the result title row holds the copy control the arguments title holds",
    );
    assertEquals(
      count(body, "border-t"),
      1,
      "one rule marks where the result half begins",
    );
  },
);
