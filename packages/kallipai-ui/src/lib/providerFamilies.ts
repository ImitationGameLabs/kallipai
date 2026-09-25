// Families the tagma backend knows how to build. Both provider-create
// paths -- the profiles-page dialog and the settings vault form -- pick
// from this one list, so a new backend family lands everywhere by
// editing this array (and creates stay inside what an instance can
// actually run).
export const MODEL_PROVIDER_FAMILIES = [
  "deepseek",
  "openai-compatible",
] as const;
