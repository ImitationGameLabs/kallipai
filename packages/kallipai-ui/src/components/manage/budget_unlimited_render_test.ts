import { assert } from "@std/assert";

// Source-read pins for the unlimited forms of the manage budget surfaces
// (same rationale as chrome_pins_test): the badge/hide contracts are
// template behavior a typecheck cannot see, and the adjust card must
// reduce to a Set-only form under an unlimited budget.

const BAR = new URL("./BudgetBar.svelte", import.meta.url);
const PAGE = new URL("../../pages/manage/BudgetPage.svelte", import.meta.url);
const CARD = new URL("./AgentStatusCard.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "BudgetBar renders the unlimited badge and hides the fill",
  { permissions: { read: [BAR] } },
  () => {
    const src = source(BAR);
    assert(
      src.includes("{#if unlimited}"),
      "the badge branch must be gated on unlimited",
    );
    assert(
      src.includes("aria-valuetext={unlimited ?"),
      "the unlimited track must be announced to assistive tech",
    );
    assert(
      src.includes("{#if !unlimited}"),
      "the fill must be hidden when unlimited",
    );
  },
);

Deno.test(
  "BudgetPage quick adjust disappears and the bar badges when unlimited",
  { permissions: { read: [PAGE] } },
  () => {
    const src = source(PAGE);
    assert(
      src.includes("{#if !budgetStore.unlimited}"),
      "quick adjust and the increase/decrease pair must be gated on !unlimited",
    );
    assert(
      src.includes("unlimited={budgetStore.unlimited}"),
      "the page's bar must badge like the overview's",
    );
    assert(
      src.includes("if (budgetStore.unlimited) onSet()"),
      "Enter must route to Set while unlimited",
    );
    assert(
      src.includes("manage_budget_unlimited_hint()"),
      "the unlimited hint must replace the adjust hint",
    );
  },
);

Deno.test(
  "AgentStatusCard forwards unlimited to its BudgetBar",
  { permissions: { read: [CARD] } },
  () => {
    const src = source(CARD);
    assert(
      src.includes("unlimited?: boolean"),
      "the card must accept the unlimited prop",
    );
    assert(
      src.includes("{unlimited}"),
      "the card must forward unlimited to BudgetBar",
    );
  },
);
