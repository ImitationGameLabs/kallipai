// Pins MODEL_PROVIDER_FAMILIES to the four wire families the gateway
// admits. The backend authority is WIRE_FAMILIES in
// crates/platform/kallipai-model-gateway/src/forward/dialect.rs; the UI
// side cannot import Rust, so this literal pin plus that file's own
// dev-dependency test keep the two faces from drifting apart.

import { assertEquals } from "@std/assert";
import { MODEL_PROVIDER_FAMILIES } from "./providerFamilies.ts";

Deno.test("provider families cover the gateway's four wire families", () => {
  assertEquals(
    [...MODEL_PROVIDER_FAMILIES],
    ["deepseek", "openai-compatible", "openai-responses", "anthropic"],
  );
});
