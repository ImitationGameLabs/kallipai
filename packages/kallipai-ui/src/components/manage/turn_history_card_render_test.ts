import { assert } from "@std/assert";

// Source-read pins for the turn-history card (same rationale as the other
// render pins: these are template contracts a typecheck cannot see). The
// decision layer is covered by its unit tests; what is pinned here is the
// card's own side -- which state it shows for a given answer, the two
// honesty boundaries the read face imposes (stored text is shown as
// text; a message row has no full-read control, since no such route
// exists), and one visible branch per empty, failed and paged state.
//
// The answers a swap can orphan are pinned here as well: whether an
// answer still belongs is a template contract a typecheck cannot see,
// and the card is where each read asks it.

const CARD = new URL("./TurnHistoryCard.svelte", import.meta.url);
const PAGE = new URL(
  "../../pages/manage/AgentDetailPage.svelte",
  import.meta.url,
);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

/** The card's source between two markers, so a pin can speak of one path. */
function between(src: string, from: string, to: string): string {
  const start = src.indexOf(from);
  const end = src.indexOf(to, start + 1);
  assert(start > 0, `${from} must appear in the card`);
  assert(end > start, `${to} must follow ${from} in the card`);
  return src.slice(start, end);
}

function count(hay: string, needle: string): number {
  return hay.split(needle).length - 1;
}

Deno.test(
  "turn card renders stored text as text, spliced by byte range",
  { permissions: { read: [CARD] } },
  () => {
    const src = source(CARD);
    assert(
      !/\{@html\s/.test(src),
      "stored text must never be injected as markup",
    );
    assert(!src.includes("href"), "stored text must not become a link");
    assert(
      !src.includes("canReadFull"),
      "the message view offers no full read: it must not be wired to a control",
    );
    assert(
      src.includes("backend.callBody(id, turnId, index, offset)"),
      "a body range must be read through the backend's callBody",
    );
    for (const call of [
      "manage_turns_body_range({",
      "manage_turns_body_more()",
      "manage_turns_body_end()",
      "manage_turns_body_empty()",
      "manage_turns_body_reclaimed({",
    ]) {
      assert(src.includes(call), `the stored-text block must call ${call}`);
    }
  },
);

Deno.test(
  "turn card keeps no-log, empty and failed apart",
  { permissions: { read: [CARD] } },
  () => {
    const src = source(CARD);
    assert(
      src.includes('listFire === "no-log"') &&
        src.includes("manage_turns_no_log()"),
      "a list 404 must render the no-log line",
    );
    assert(
      src.includes("manage_turns_empty()"),
      "an empty page must render the empty line",
    );
    assert(
      src.includes('listFire === "failed"') &&
        src.includes("manage_turns_load_failed()"),
      "a failed read must render the failure line",
    );
    assert(
      src.includes('errorState("list", errorStatus(e))'),
      "the list failure must be classified by the decision layer",
    );
    assert(
      src.includes('errorState("detail", errorStatus(e))'),
      "a vanished turn must be classified, not matched against server prose",
    );
    assert(
      src.includes("displayError("),
      "a failed read shows the server's own message, never a fixed sentence",
    );
  },
);

Deno.test(
  "turn card folds reasoning and keeps it out of the DOM until opened",
  { permissions: { read: [CARD] } },
  () => {
    const src = source(CARD);
    assert(
      src.includes("manage_turns_reasoning_show()") &&
        src.includes("manage_turns_reasoning_hide()"),
      "the reasoning fold needs both labels",
    );
    assert(
      src.includes("manage_turns_reasoning_unavailable()"),
      "a flag without text must say so instead of folding nothing",
    );
    const gate = src.indexOf("{#if reasoningOpen(turn.turn_id, index)}");
    const body = src.indexOf("{thought.text}");
    assert(gate > 0, "the reasoning text must sit behind its own open gate");
    assert(body > gate, "the reasoning text must not render while folded");
  },
);

Deno.test(
  "turn card pages older turns with the cursor it was given",
  { permissions: { read: [CARD] } },
  () => {
    const src = source(CARD);
    assert(
      src.includes("limit: TURNS_PAGE_LIMIT"),
      "a page asks for the default row count",
    );
    assert(
      src.includes("before: cursor"),
      "an older page is asked for by cursor",
    );
    assert(
      src.includes("manage_turns_load_more()"),
      "the paging control must render its label",
    );
    assert(
      src.includes("canLoadMore(page, loadingMore)"),
      "the paging control must follow the decision layer's verdict",
    );
  },
);

Deno.test(
  "agent detail mounts the turn card inside the status block",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    assert(
      src.includes("<TurnHistoryCard {id} {backend} />"),
      "the detail page must mount the card with the agent id and its backend",
    );
  },
);

Deno.test(
  "turn card asks whether an answer still belongs before it writes",
  { permissions: { read: [CARD] } },
  () => {
    const src = source(CARD);
    const swap = between(
      src,
      "if (loadedFor === id) return;",
      "void loadFirst();",
    );
    assert(
      swap.includes("readEpoch += 1;"),
      "a swap must start a new era, or the old agent's answers still land",
    );
    for (const flag of [
      "loading = false;",
      "loadingMore = false;",
      "detailLoading = false;",
    ]) {
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
        "async function toggleTurn(",
        "rows = mergePages(rows, next)",
      ],
      [
        "async function toggleTurn(",
        "async function toggleBody(",
        "detail = read;",
      ],
    ];
    for (const [from, to, write] of paths) {
      const path = between(src, from, to);
      assert(
        count(path, ask) >= 3,
        `${from} lost a guard, or a guard changed shape: move this pin with it`,
      );
      // The name says before it writes, so the order is checked too.
      assert(
        path.indexOf(ask) >= 0 && path.indexOf(write) > path.indexOf(ask),
        `${from} must ask before ${write}`,
      );
    }
    assert(
      count(src, ask) === 9,
      "three reads, three asks each: a new guard means this pin moves too",
    );
    // One read of the list at a time, in the same words at both entries,
    // and the same disable on both controls.
    const reads = "if (loading || loadingMore) return;";
    assert(
      between(
        src,
        "async function loadFirst()",
        "async function loadMore()",
      ).includes(reads),
      "a refresh must turn the ask away while a read is in flight",
    );
    assert(
      between(
        src,
        "async function loadMore()",
        "async function toggleTurn(",
      ).includes(reads),
      "a paging ask must turn away while any read is in flight",
    );
    assert(
      count(src, "disabled={loading || loadingMore}") === 3,
      "both controls disable on either read in flight",
    );
  },
);

Deno.test(
  "turn card keeps a body answer tied to the block that asked",
  { permissions: { read: [CARD] } },
  () => {
    const src = source(CARD);
    assert(
      src.includes("const mine = () => bodies[key]?.token === token;"),
      "a block must remember the read that is filling it",
    );
    assert(
      count(src, "if (!mine()) return;") === 2,
      "both the answer and the failure must ask before writing",
    );
    assert(
      count(src, "bodyToken += 1;") === 1,
      "each read must take a fresh token",
    );
  },
);
