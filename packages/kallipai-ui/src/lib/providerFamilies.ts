// Families the tagma backend knows how to build. Every provider-create
// dialog picks from this one list, so a new backend family lands
// everywhere by editing this array (and creates stay inside what an
// instance can actually run).
// The alignment anchor is the gateway's WIRE_FAMILIES
// (crates/platform/kallipai-model-gateway/src/forward/dialect.rs);
// keep this list in that declaration order.
export const MODEL_PROVIDER_FAMILIES = [
  "deepseek",
  "openai-compatible",
  "openai-responses",
  "anthropic",
] as const;
