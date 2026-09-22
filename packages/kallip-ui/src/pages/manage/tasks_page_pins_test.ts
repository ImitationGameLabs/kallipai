import { assert, assertEquals } from "@std/assert";
import zhMessages from "../../../i18n/project.inlang/messages/zh/manage_tasks.json" with { type: "json" };
import enMessages from "../../../i18n/project.inlang/messages/en/manage_tasks.json" with { type: "json" };

// Source pins for the TasksPage confirmation roster: the component must
// derive confirmer state through the shared pure helpers (the cycle
// boundary lives in one place, mirroring kallip-task gates.rs), not
// re-implement it inline.
const PAGE = new URL("./TasksPage.svelte", import.meta.url);
const COMPUTE = new URL("../../lib/manage/compute.ts", import.meta.url);
const STORE = new URL("../../lib/manage/tasks.svelte.ts", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "TasksPage derives confirmer confirmations via the shared helpers",
  { permissions: { read: [PAGE, COMPUTE] } },
  () => {
    const src = source(PAGE);
    assert(
      src.includes("confirmerConfirmationState("),
      "the page must call confirmerConfirmationState, not inline the confirmation scan",
    );
    assert(
      !src.includes("confirmationBoundary("),
      "the boundary math stays in compute.ts, not the component",
    );
  },
);

Deno.test(
  "compute exports the confirmation-cycle helpers",
  { permissions: { read: [COMPUTE] } },
  () => {
    const src = source(COMPUTE);
    assert(
      src.includes("export function confirmationBoundary("),
      "confirmationBoundary must be exported for the page and tests",
    );
    assert(
      src.includes("export function confirmerConfirmationState("),
      "confirmerConfirmationState must be exported for the page and tests",
    );
  },
);

Deno.test(
  "TasksPage pagination goes through the store's clamped jump",
  { permissions: { read: [PAGE, STORE, COMPUTE] } },
  () => {
    const page = source(PAGE);
    const store = source(STORE);
    const compute = source(COMPUTE);
    assert(
      page.includes("tasksStore.goToPage("),
      "pagination buttons must jump via the store, not inline fetch calls",
    );
    assert(
      store.includes("clampPage("),
      "the store must clamp page jumps through the shared helper",
    );
    assert(
      compute.includes("export function clampPage("),
      "clampPage must be exported for the store and tests",
    );
    assert(
      !store.includes("Math.ceil"),
      "page math lives in clampPage, not inline in the store",
    );
    assert(
      store.includes("clampPage(this.page,"),
      "refresh must re-clamp the page against the fresh total",
    );
  },
);

Deno.test(
  "TasksPage expands filed confirmations and separates the empty states",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    assert(
      src.includes("expandedConfirmer"),
      "confirmation expansion must be a component state toggle",
    );
    assert(
      src.includes("manage_tasks_confirm_no_report()"),
      "a filed confirmation without a report needs its own message, not the pending one",
    );
  },
);

Deno.test("manage_tasks message keys stay in parity across locales", () => {
  const zh = Object.keys(zhMessages).filter((k) => k !== "$schema");
  const en = Object.keys(enMessages).filter((k) => k !== "$schema");
  assertEquals(zh, en);
});

Deno.test(
  "TasksPage wires the three download buttons through the store",
  { permissions: { read: [PAGE, STORE] } },
  () => {
    const page = source(PAGE);
    const store = source(STORE);
    // Every button action routes through a store wrapper, never a bare
    // backend call in the component.
    assert(page.includes("tasksStore.fetchArchive("), "archive button");
    assert(page.includes("tasksStore.exportTask("), "export button");
    assert(page.includes("tasksStore.exportAllTasks("), "bulk button");
    assert(store.includes("fetchArchive"), "store archive wrapper");
    assert(store.includes("exportTask"), "store export wrapper");
    assert(store.includes("exportAllTasks"), "store bulk wrapper");
    // The capability flag drives the disabled state on all three.
    const disabled = page.match(/disabled=\{[^}]*downloadsAvailable[^}]*\}/g);
    assert(
      disabled !== null && disabled.length >= 3,
      "three disabled bindings",
    );
    // The offline hint renders from the same flag.
    assert(page.includes("manage_tasks_downloads_offline()"), "hint row");
  },
);

Deno.test(
  "refresh guards superseded responses without starving polling",
  { permissions: { read: [STORE] } },
  () => {
    const store = source(STORE);
    // The seq token exists and is checked before writing rows.
    assert(store.includes("private refreshSeq = 0;"), "token field");
    assert(store.includes("if (superseded()) return;"), "success guard");
    assert(
      store.includes("if (seq === this.refreshSeq) this.isLoading = false;"),
      "conditional clear",
    );
    // Ordering: the isLoading short-circuit must precede the increment,
    // otherwise a tick that bails on isLoading still bumps the token and
    // starves every later poll.
    const gate = store.indexOf("if (!force && this.isLoading) return;");
    const bump = store.indexOf("const seq = ++this.refreshSeq;");
    assert(gate !== -1 && bump !== -1, "both lines present");
    assert(gate < bump, "isLoading short-circuit must precede ++refreshSeq");
  },
);

Deno.test(
  "TasksPage renders the ledger details via pure helpers",
  { permissions: { read: [PAGE, COMPUTE] } },
  () => {
    const page = source(PAGE);
    const compute = source(COMPUTE);
    // Filter chips route through the store setter.
    assert(page.includes("statusChips"), "chip array is pinned in the page");
    assert(page.includes("tasksStore.setStatus("), "chip click setter");
    // The vote badge counts filed confirmations over the roster.
    assert(page.includes("confirmedCount"), "badge numerator");
    assert(page.includes(".confirmers.length"), "badge denominator");
    // Timeline payloads narrow through the pure helper, not inline casts.
    assert(page.includes("timelinePayloadView("), "payload narrowing");
    assert(!page.includes("as { note"), "no inline payload casts in the page");
    // Association keys render through the pure formatter.
    assert(page.includes("associationText("), "association formatter");
    assert(
      compute.includes("export function associationText("),
      "helper exported",
    );
  },
);
