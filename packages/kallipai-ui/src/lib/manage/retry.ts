// Relative-time bucketing and error normalization for the agent detail
// page's retry card. Pure functions only: locale rendering (which message
// key, which plural form) stays in the component, so these stay testable
// without a rune runtime (compute.ts pattern).

export type RelativeKind = "just" | "min" | "hour" | "day";

export interface RelativeTime {
  readonly kind: RelativeKind;
  readonly n: number;
}

/** Bucket a unix-seconds `ts` against `now`: <1min "just", then min/hour/day. */
export function relativeTime(now: number, ts: number): RelativeTime {
  // Clock skew or a retry logged in the same second reads as "just now"
  // rather than a negative bucket.
  const elapsed = Math.max(0, now - ts);
  const minutes = Math.floor(elapsed / 60);
  if (minutes < 1) return { kind: "just", n: 0 };
  if (minutes < 60) return { kind: "min", n: minutes };
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return { kind: "hour", n: hours };
  return { kind: "day", n: Math.floor(hours / 24) };
}

export type RetryErrorKind =
  | "network"
  | "timeout"
  | "rate_limit"
  | "auth"
  | "unknown";

// Order encodes precedence: specific causes first, the broad network
// family last, so "connection timed out" reads as timeout and a bare
// "connection error" still lands in network.
const ERROR_PATTERNS: readonly (readonly [RetryErrorKind, RegExp])[] = [
  ["timeout", /timeout|timed out|deadline/i],
  ["rate_limit", /rate.?limit|too many requests|\b429\b/i],
  ["auth", /unauthorized|forbidden|api key|\b40[13]\b/i],
  ["network", /connection|network|dns|socket|econnrefused|proxy/i],
];

/** Map a raw transport error string to a short display kind. */
export function classifyRetryError(error: string): RetryErrorKind {
  for (const [kind, pattern] of ERROR_PATTERNS) {
    if (pattern.test(error)) return kind;
  }
  return "unknown";
}
