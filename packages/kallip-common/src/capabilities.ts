import type { TranscriptLine } from "./transcript.ts";

// Declares what a Session can actually deliver. The UI branches on these to
// degrade gracefully: under agora there are no streaming deltas, tool events,
// or retry/failover telemetry, so the corresponding controls hide. Approvals
// ride the herald tunnel as opaque bytes, so they are available on every
// transport.

export interface SessionCapabilities {
  /** Streaming word-level deltas (assistantContentDelta / reasoningDelta). */
  readonly streamingDeltas: boolean;
  /** toolCall / toolResult events. */
  readonly toolEvents: boolean;
  /** Interactive approvals (approvalUpdated + the approvals API). */
  readonly approvals: boolean;
  /** retry / failover / streamReset telemetry. */
  readonly retryTelemetry: boolean;
  /** Management surface exists (status, budget, interrupt, policy). */
  readonly management: boolean;
  /** Line kinds the transcript will ever contain on this transport. */
  readonly lineKinds: ReadonlySet<TranscriptLine["kind"]>;
}

const DIRECT_KINDS: ReadonlySet<TranscriptLine["kind"]> = new Set([
  "user",
  "assistant",
  "reasoning",
  "toolCall",
  "toolResult",
  "status",
  "error",
  "system",
  "retrying",
  "failover",
  "failoverExhausted",
  "streamDropped",
]);

const AGORA_KINDS: ReadonlySet<TranscriptLine["kind"]> = new Set([
  "user",
  "assistant",
  "status",
  "error",
  "system",
]);

export const DIRECT_CAPABILITIES: SessionCapabilities = {
  streamingDeltas: true,
  toolEvents: true,
  approvals: true,
  retryTelemetry: true,
  management: true,
  lineKinds: DIRECT_KINDS,
};

export const AGORA_CAPABILITIES: SessionCapabilities = {
  streamingDeltas: false,
  toolEvents: false,
  approvals: true,
  retryTelemetry: false,
  management: false,
  lineKinds: AGORA_KINDS,
};
