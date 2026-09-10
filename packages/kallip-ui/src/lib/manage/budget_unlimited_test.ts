// Tests for the budget store's unlimited-budget surface: the flag rides the
// GET response, an unlimited budget is never "paused", never shows an ETA,
// and never shows a consumed percentage; setUnlimited flips optimistically
// (zeroing budget/remaining) and reverts on failure; a finite set_remaining
// migrates back to a limited budget with the consumed value intact.
//
// Seam note: the module is rune-bearing but `deno test` runs it uncompiled,
// so a passthrough $state shim (the channels_openBudget_test pattern) lets
// the store run with plain fields. The backend is a stub carrying only the
// two budget methods; polling is never started, so no timers are involved.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals, assertRejects } = await import("@std/assert");
const { budgetStore } = await import("./budget.svelte.ts");
const { burnRate } = await import("./compute.ts");
type BudgetResponse = import("@kallipai/kallip-client").BudgetResponse;
type BudgetUpdateRequest =
  import("@kallipai/kallip-client").BudgetUpdateRequest;
type ManagementBackend = import("./backend.ts").ManagementBackend;

function resp(
  budget: number,
  consumed: number,
  remaining: number,
  unlimited = false,
): BudgetResponse {
  return { budget, consumed, remaining, unlimited };
}

function backendWith(
  get: BudgetResponse,
  update?: (body: BudgetUpdateRequest) => Promise<BudgetResponse>,
): ManagementBackend {
  return {
    getBudget: () => Promise.resolve(get),
    updateBudget:
      update ?? (() => Promise.reject(new Error("unexpected mutation"))),
  } as unknown as ManagementBackend;
}

Deno.test(
  "an unlimited GET response reads as unpaused with no pct and no ETA",
  async () => {
    budgetStore.switchBackend(backendWith(resp(0, 1_234, 0, true)));
    await budgetStore.refresh();
    assertEquals(budgetStore.unlimited, true);
    // Wire zeroes for budget/remaining must not read as "cleared/paused".
    assertEquals(budgetStore.isPaused, false);
    assertEquals(budgetStore.consumedPct, 0);
    // Even with an active burn rate (a finite budget would show an ETA), an
    // unlimited budget reports none.
    (
      budgetStore as unknown as {
        samples: { consumed: number; timestamp: number }[];
      }
    ).samples = [
      { consumed: 1_000, timestamp: 0 },
      { consumed: 2_000, timestamp: 60_000 },
    ];
    assertEquals(
      burnRate(
        (
          budgetStore as unknown as {
            samples: { consumed: number; timestamp: number }[];
          }
        ).samples,
      ) === null,
      false,
    );
    assertEquals(budgetStore.etaMinutes, null);
  },
);

Deno.test(
  "setUnlimited flips optimistically and reverts on failure",
  async () => {
    budgetStore.switchBackend(backendWith(resp(100, 40, 60)));
    await budgetStore.refresh();
    assertEquals(budgetStore.unlimited, false);
    const p = budgetStore.setUnlimited();
    // Optimistic state is visible before the response lands.
    assertEquals(budgetStore.unlimited, true);
    assertEquals(budgetStore.budget, 0);
    assertEquals(budgetStore.remaining, 0);
    assertEquals(budgetStore.consumed, 40);
    await assertRejects(() => p);
    // Failure reverts to the limited budget.
    assertEquals(budgetStore.unlimited, false);
    assertEquals(budgetStore.budget, 100);
    assertEquals(budgetStore.remaining, 60);
    assertEquals(budgetStore.consumed, 40);
  },
);

Deno.test("a successful setUnlimited applies the server response", async () => {
  budgetStore.switchBackend(
    backendWith(resp(100, 40, 60), (body) => {
      assertEquals(body, { set_unlimited: true });
      return Promise.resolve(resp(0, 175, 0, true));
    }),
  );
  await budgetStore.refresh();
  await budgetStore.setUnlimited();
  assertEquals(budgetStore.unlimited, true);
  assertEquals(budgetStore.budget, 0);
  assertEquals(budgetStore.remaining, 0);
  // Consumption keeps its true accumulated value across the switch.
  assertEquals(budgetStore.consumed, 175);
});

Deno.test(
  "a finite set_remaining migrates an unlimited budget back to limited",
  async () => {
    budgetStore.switchBackend(
      backendWith(resp(0, 175, 0, true), (body) => {
        assertEquals(body, { set_remaining: 80 });
        // Server-side migration: budget = consumed + value.
        return Promise.resolve(resp(255, 175, 80, false));
      }),
    );
    await budgetStore.refresh();
    assertEquals(budgetStore.unlimited, true);
    await budgetStore.setRemaining(80);
    assertEquals(budgetStore.unlimited, false);
    assertEquals(budgetStore.budget, 255);
    assertEquals(budgetStore.remaining, 80);
    assertEquals(budgetStore.consumed, 175);
  },
);
