// Line formatting for the agent detail page's retry card: outcome wording,
// the absolute/relative retry line, and the classified-error display label.
// retry.ts owns bucketing (relativeTime) and the string->kind mapping
// (classifyRetryError); this file owns kind->message and the assembled line
// (i18n via paraglide -- same-layer precedent as compute.ts/profiles-view).
// `now` is an explicit parameter so tests drive the clock; RetryListCard
// passes Date.now()/1000 per render, which the 5s status poll refreshes anyway.

import { formatStampInZone } from "../time/stamp.ts";
import {
  manage_agent_retry_days_ago,
  manage_agent_retry_error_auth,
  manage_agent_retry_error_network,
  manage_agent_retry_error_rate_limit,
  manage_agent_retry_error_timeout,
  manage_agent_retry_error_unknown,
  manage_agent_retry_exhausted,
  manage_agent_retry_hours_ago,
  manage_agent_retry_just_now,
  manage_agent_retry_line,
  manage_agent_retry_minutes_ago,
  manage_agent_retry_retried,
} from "../../paraglide/messages.js";
import {
  classifyRetryError,
  relativeTime,
  type RetryErrorKind,
} from "./retry.ts";

export interface RetryEntry {
  timestamp: number;
  attempt: number;
  max_attempts: number;
  error: string;
}

/** "retried" until the attempt number hits the cap, then "exhausted". */
export function retryOutcome(r: RetryEntry): string {
  return r.attempt < r.max_attempts
    ? manage_agent_retry_retried()
    : manage_agent_retry_exhausted();
}

/** The retry line with an absolute locale timestamp and the raw error. */
export function fmtAbsoluteRetry(
  r: RetryEntry,
  timezone?: string | null,
): string {
  return manage_agent_retry_line({
    date: formatStampInZone(r.timestamp * 1000, undefined, timezone),
    error: r.error,
    outcome: retryOutcome(r),
  });
}

// Display labels for the classified error kinds (retry.ts owns the
// string->kind mapping; this maps kind->message).
const errorLabels: Record<RetryErrorKind, () => string> = {
  network: manage_agent_retry_error_network,
  timeout: manage_agent_retry_error_timeout,
  rate_limit: manage_agent_retry_error_rate_limit,
  auth: manage_agent_retry_error_auth,
  unknown: manage_agent_retry_error_unknown,
};

/** The retry line with a relative bucket (against `now`, unix seconds) and
 *  the classified error label. */
export function fmtRelativeRetry(r: RetryEntry, now: number): string {
  const { kind, n } = relativeTime(now, r.timestamp);
  const date =
    kind === "just"
      ? manage_agent_retry_just_now()
      : kind === "min"
        ? manage_agent_retry_minutes_ago({ n })
        : kind === "hour"
          ? manage_agent_retry_hours_ago({ n })
          : manage_agent_retry_days_ago({ n });
  return manage_agent_retry_line({
    date,
    error: errorLabels[classifyRetryError(r.error)](),
    outcome: retryOutcome(r),
  });
}
