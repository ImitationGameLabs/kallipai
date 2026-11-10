import { assert, assertEquals, assertFalse } from "@std/assert";

// Source-read pins for the two gateway profile dialogs. The dialogs
// share one contract: an empty or text-only modality selection must
// null out in the payload, a stored provider or effort value must
// stay selectable even when it left the pool, the tag group seeds
// from the intersection of the stored list with the wire modalities,
// and the four limit fields are parsed once by the shared rule that
// the submit guard and the payload both read. The create form's
// starting collection is dialog-side: a pinned one seeds the field and
// locks the picker, a null one leaves it blank and unsubmittable until
// the user picks. The page's call site is pinned by the page's own test.
const USER_DIALOG = new URL("./UserProfileDialog.svelte", import.meta.url);
const MANAGE_DIALOG = new URL(
  "../manage/gateway/ProfileDialog.svelte",
  import.meta.url,
);
const COMPUTE_LIB = new URL("../../lib/manage/compute.ts", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

/** The file's code lines: a line whose first non-blank characters open a
 * comment (`//`, `/*`, `*`, `<!--`) drops out -- the shape of an edit made
 * while debugging and left behind. Three limits, all deliberate: a
 * trailing comment is not stripped, so a pinned shape copied to the end of
 * another line still matches; the inner lines of a block or markup comment
 * are kept unless they lead with `*`, so a block that was commented out
 * still reads as code unless its lines open with a marker; and a code line
 * that begins with `*` drops out too (a wrapped multiplication, a `*`
 * selector in `<style>`), which costs a red pin and never a green one.
 * Nothing here tracks string or regex state, so neither an apostrophe in
 * the markup nor a `/[//]/` literal can switch the filter off. */
function codeLinesOf(text: string): string {
  return text
    .split("\n")
    .filter((line) => {
      const rest = line.trimStart();
      return !(
        rest.startsWith("//") ||
        rest.startsWith("/*") ||
        rest.startsWith("*") ||
        rest.startsWith("<!--")
      );
    })
    .join("\n");
}

function codeLines(url: URL): string {
  return codeLinesOf(source(url));
}

Deno.test("a pin reads code lines, never a commented-out copy", () => {
  const dialog = [
    "<p>Don't edit this row.</p>",
    "<script>",
    "  if (!canSubmit) return; // kept, a trailing comment stays",
    "  // if (!canSubmit) return;",
    "  /* const x = parseProfileLimit(raw); */",
    "</script>",
  ].join("\n");
  const code = codeLinesOf(dialog);
  assert(code.includes("if (!canSubmit) return; // kept"));
  assertFalse(code.includes("// if (!canSubmit) return;"));
  assertFalse(code.includes("const x = parseProfileLimit(raw);"));
  // The documented limit: a trailing comment is not stripped, so a shape
  // copied into one still matches. A whole-line comment-out does not.
  assert(
    codeLinesOf("a(); // if (!canSubmit) return;").includes(
      "if (!canSubmit) return;",
    ),
  );
  // The other two: a block or markup comment keeps its inner lines, and a
  // code line that leads with `*` drops out with the comment lines.
  assert(codeLinesOf("/*\nx\n*/").includes("x"));
  assert(codeLinesOf("<!--\nx\n-->").includes("x"));
  assertFalse(codeLinesOf("  * 100;").includes("100;"));
});

const NULLING_CHAIN = "normalizeModalities(modalityDraft)?.slice() ?? null";
const KEPT_PROVIDER =
  '{#if providerDraft !== "" && !providers.some((p) => p.provider_id === providerDraft)}';
const KEPT_EFFORT =
  '{#if effortDraft !== "" && !EFFORT_OPTIONS.includes(effortDraft as ReasoningEffort)}';
const INTERSECTION_SEED = "MODALITY_ORDER.filter((m) => stored.includes(m))";

Deno.test(
  "an empty or text-only modality selection nulls out in the payload",
  { permissions: { read: [USER_DIALOG, MANAGE_DIALOG, COMPUTE_LIB] } },
  () => {
    // Behavior: the shared normalizer collapses both to undefined.
    // Chain: the dialogs write "?? null", so the wire body carries
    // null and the server stores its default (never a literal "[]").
    const compute = codeLines(COMPUTE_LIB);
    assert(
      compute.includes('modalities.every((m) => m === "text")'),
      "the normalizer must treat every-text and empty alike",
    );
    for (const url of [USER_DIALOG, MANAGE_DIALOG]) {
      const src = codeLines(url);
      assert(
        src.includes(NULLING_CHAIN),
        "the submit chain must turn the collapsed selection into null",
      );
    }
  },
);

Deno.test(
  "a stored provider or effort value stays selectable in both dialogs",
  { permissions: { read: [USER_DIALOG, MANAGE_DIALOG] } },
  () => {
    for (const url of [USER_DIALOG, MANAGE_DIALOG]) {
      const src = codeLines(url);
      assert(
        src.includes(KEPT_PROVIDER),
        "a provider that left the pool must keep its kept option",
      );
      assert(
        src.includes(KEPT_EFFORT),
        "an effort outside the enum must keep its kept option",
      );
    }
  },
);

Deno.test(
  "the tag group seeds from the stored-versus-wire intersection",
  { permissions: { read: [USER_DIALOG, MANAGE_DIALOG] } },
  () => {
    for (const url of [USER_DIALOG, MANAGE_DIALOG]) {
      const src = codeLines(url);
      assert(
        src.includes("function normalizeModalitiesInit("),
        "the dialog must seed the tag group through the shared shape",
      );
      assert(
        src.includes(INTERSECTION_SEED),
        "seeding must intersect the stored list with the wire modalities",
      );
    }
  },
);

Deno.test(
  "the manage dialog derives the family line from the chosen provider",
  { permissions: { read: [MANAGE_DIALOG] } },
  () => {
    const src = codeLines(MANAGE_DIALOG);
    assert(
      src.includes("familyShown = $derived"),
      "the family line must track the provider through a derived",
    );
  },
);

Deno.test(
  "an empty provider pool shows a create-first hint in create mode",
  { permissions: { read: [USER_DIALOG, MANAGE_DIALOG] } },
  () => {
    for (const url of [USER_DIALOG, MANAGE_DIALOG]) {
      const src = codeLines(url);
      assert(
        src.includes("{#if creating && providers.length === 0}"),
        "the hint must render only when creating with an empty pool",
      );
    }
  },
);

Deno.test(
  "the manage create form starts blank on the list, seeded on a detail",
  { permissions: { read: [MANAGE_DIALOG] } },
  () => {
    const dialog = codeLines(MANAGE_DIALOG);
    // One prop carries both sides: a name seeds the field and locks the
    // picker, null leaves the list entry blank. The page's call site is
    // pinned by the page's own test.
    assert(
      dialog.includes(
        'collectionDraft = row?.collection_name ?? pinnedCollection ?? "";',
      ),
      "the create form seeds from the row or a pinned collection",
    );
    assert(
      dialog.includes("disabled={!creating || pinnedCollection !== null}"),
      "a pinned collection is not the user's to change",
    );
    // With no collection picked (the list entry's blank start) submit
    // stays disabled.
    assert(
      dialog.includes('collectionDraft.trim() !== ""'),
      "an empty collection keeps submit disabled",
    );
  },
);

Deno.test(
  "both dialogs check the four limit fields through the shared rule",
  { permissions: { read: [USER_DIALOG, MANAGE_DIALOG] } },
  () => {
    // Each draft is parsed once, into a derived the guard and the payload
    // both read. The derived line is pinned verbatim, so folding a parsed
    // value back to null there turns the pin red; the count pins that all
    // four reach the shared rule; the payload lines pin the wire side,
    // where a re-parse with Number(...) would pass a blank field as 0 and
    // text as NaN; the local-definition half catches a copy under the
    // rule's own names.
    for (const url of [USER_DIALOG, MANAGE_DIALOG]) {
      const src = codeLines(url);
      assert(
        src.includes("lib/manage/gateway/profileLimits.ts"),
        "the dialogs must import the rule from its shared home",
      );
      assertEquals(
        src.split("parseProfileLimit(").length - 1,
        4,
        "all four limit fields must go through the shared rule",
      );
      for (const [derived, draft, wire] of [
        [
          "const contextLimit = $derived(parseProfileLimit(contextDraft));",
          "contextDraft",
          "max_context_window: contextLimit,",
        ],
        [
          "const budgetLimit = $derived(parseProfileLimit(budgetDraft));",
          "budgetDraft",
          "max_budget: budgetLimit,",
        ],
        [
          "const tpmLimit = $derived(parseProfileLimit(tpmDraft));",
          "tpmDraft",
          "tpm_limit: tpmLimit,",
        ],
        [
          "const rpmLimit = $derived(parseProfileLimit(rpmDraft));",
          "rpmDraft",
          "rpm_limit: rpmLimit,",
        ],
      ]) {
        assert(src.includes(derived), derived);
        assert(src.includes(draft), draft);
        assert(
          src.includes(wire),
          "the payload must carry the parsed field, not a re-parse",
        );
      }
      assert(
        src.includes("if (!canSubmit || busy) return;"),
        "submit must stay gated before the payload",
      );
      assert(
        src.includes("contextLimit === undefined ||"),
        "the guard must tell unreadable text from a blank field",
      );
      for (const local of [
        "function parseProfileLimit(",
        "function numberOrNull(",
        "function numeric(",
      ]) {
        assertFalse(src.includes(local), local);
      }
    }
  },
);
