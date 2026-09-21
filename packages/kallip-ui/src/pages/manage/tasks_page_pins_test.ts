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
