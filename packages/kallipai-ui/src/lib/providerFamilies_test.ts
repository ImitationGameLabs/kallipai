// Pins MODEL_PROVIDER_FAMILIES to the four wire families the gateway
// admits, order included: assertEquals compares the whole array, so a
// renamed or moved entry fails here. That makes the pin a literal
// snapshot, which is the honest shape for a data constant the provider
// dialogs render: there is no derived behaviour to exercise, and its
// content can only be read against WIRE_FAMILIES in
// crates/platform/kallipai-model-gateway/src/forward/dialect.rs, which the UI
// side cannot import. A coordinated change on both sides passes here, and
// the two files are compared by hand when the families move.

import { assertEquals } from "@std/assert";
import { MODEL_PROVIDER_FAMILIES } from "./providerFamilies.ts";

Deno.test("provider families cover the gateway's four wire families", () => {
  assertEquals(
    [...MODEL_PROVIDER_FAMILIES],
    ["deepseek", "openai-compatible", "openai-responses", "anthropic"],
  );
});
