import { assert } from "@std/assert";

// Source-read pins for the attachment bar (same rationale as
// chrome_pins_test): the four state branches and the callback wiring are
// contracts a typecheck cannot see, and the bar must stay a dumb renderer
// (no upload logic of its own -- the page orchestrates).

const BAR = new URL("./AttachmentBar.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "AttachmentBar renders all four item states",
  { permissions: { read: [BAR] } },
  () => {
    const src = source(BAR);
    for (const state of ["uploading", "ready", "too_large", "failed"]) {
      assert(
        src.includes(`item.status === "${state}"`),
        `the ${state} branch must render`,
      );
    }
  },
);

Deno.test(
  "AttachmentBar wires retry and remove to the callbacks it is given",
  { permissions: { read: [BAR] } },
  () => {
    const src = source(BAR);
    assert(
      src.includes("onRetry(item.id)"),
      "retry must call the injected onRetry",
    );
    assert(
      src.includes("onRemove(item.id)"),
      "remove must call the injected onRemove",
    );
  },
);

Deno.test(
  "AttachmentBar stays a dumb renderer (no fetch of its own)",
  { permissions: { read: [BAR] } },
  () => {
    const src = source(BAR);
    assert(!src.includes("fetch("), "the bar must not speak HTTP");
    assert(
      !src.includes("FilesClient"),
      "the bar must not depend on the files client",
    );
  },
);
