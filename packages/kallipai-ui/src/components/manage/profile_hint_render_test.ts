import { assert } from "@std/assert";

// Source-read pins for the advisory model-list hint: it rides beside the
// verdict wherever status+detail render, never replacing them, and it is
// styled as a warning rather than an error. Template behavior a typecheck
// cannot see, the same rationale as the other render pins.

const CARD = new URL("./ProfileCard.svelte", import.meta.url);
const DIALOG = new URL("./ProfileDialog.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "ProfileCard shows the hint beside the verdict, not instead of it",
  { permissions: { read: [CARD] } },
  () => {
    const src = source(CARD);
    assert(
      src.includes("{#if report.models_hint}"),
      "the hint branch must be gated on the hint field",
    );
    const verdict = src.indexOf("probeStatusLabel(report.status)");
    const hint = src.indexOf("{#if report.models_hint}");
    assert(verdict !== -1, "the verdict line must exist to be ordered against");
    assert(hint !== -1, "the hint branch must exist to be ordered against");
    assert(
      verdict < hint,
      "the verdict line must come first, the hint after it",
    );
    assert(
      src.includes("{manage_profiles_probe_models_hint()}"),
      "the hint text comes from the shared message",
    );
    assert(
      src.includes("text-warning-500"),
      "the hint is advisory, not an error",
    );
  },
);

Deno.test(
  "ProfileDialog shows the hint beside the verdict, not instead of it",
  { permissions: { read: [DIALOG] } },
  () => {
    const src = source(DIALOG);
    assert(
      src.includes("{#if probeReport.modelsHint}"),
      "the hint branch must be gated on the hint field",
    );
    const verdict = src.indexOf("{probeReport.status}");
    const hint = src.indexOf("{#if probeReport.modelsHint}");
    assert(verdict !== -1, "the verdict line must exist to be ordered against");
    assert(hint !== -1, "the hint branch must exist to be ordered against");
    assert(
      verdict < hint,
      "the verdict line must come first, the hint after it",
    );
    assert(
      src.includes("{manage_profiles_probe_models_hint()}"),
      "the hint text comes from the shared message",
    );
    assert(
      src.includes("text-warning-500"),
      "the hint is advisory, not an error",
    );
  },
);
