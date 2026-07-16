import type { DomainEvent, FailoverChainExhaustion } from "./event.ts";
import { failoverChainExhaustionToProse } from "./event.ts";

// The transcript line model. Adapted from kallip-tui's ChatLine (not a 1:1 port:
// the TUI tracks streaming via two app-global flags, whereas the web port puts a
// per-line `streaming` flag on assistant/reasoning lines because Svelte
// reactivity is per-line). The reducer logic below is a faithful port of
// App::handle_sse_event.

export type TranscriptLine =
  | { readonly kind: "user"; readonly text: string }
  | {
      readonly kind: "assistant";
      readonly text: string;
      readonly streaming?: boolean;
    }
  | {
      readonly kind: "reasoning";
      readonly text: string;
      readonly streaming?: boolean;
    }
  | { readonly kind: "toolCall"; readonly name: string; readonly args: string }
  | { readonly kind: "toolResult"; readonly result: string }
  | { readonly kind: "status"; readonly text: string }
  | { readonly kind: "error"; readonly text: string }
  | { readonly kind: "system"; readonly text: string }
  | {
      readonly kind: "retrying";
      readonly attempt: number;
      readonly maxAttempts: number;
      readonly error: string;
      readonly delaySecs: number;
    }
  | {
      readonly kind: "failover";
      readonly from: string;
      readonly to: string;
      readonly reason: string;
    }
  | {
      readonly kind: "failoverExhausted";
      readonly reason: string;
      readonly detail: string;
    }
  | {
      readonly kind: "streamDropped";
      readonly attempt: number;
      readonly maxAttempts: number;
      readonly error: string;
      readonly delaySecs: number;
    };

export interface TranscriptState {
  readonly lines: TranscriptLine[];
  readonly agentBusy: boolean;
}

export const EMPTY_TRANSCRIPT: TranscriptState = {
  lines: [],
  agentBusy: false,
};

// Append a user-typed line (user input is app-side, not a DomainEvent).
export function withUserLine(
  state: TranscriptState,
  text: string,
): TranscriptState {
  return { ...state, lines: [...state.lines, { kind: "user", text }] };
}

// Clear the transcript (the /clear command).
export function clearTranscript(state: TranscriptState): TranscriptState {
  return { ...EMPTY_TRANSCRIPT, agentBusy: state.agentBusy };
}

type StreamingLineKind = "assistant" | "reasoning";

// Append a delta to the trailing streaming line of the given kind, creating one
// if none exists. Mirrors the TUI's append_streaming_delta in-place behaviour.
function appendDelta(
  lines: readonly TranscriptLine[],
  kind: StreamingLineKind,
  delta: string,
): TranscriptLine[] {
  const last = lines[lines.length - 1];
  if (last && last.kind === kind && last.streaming) {
    return [
      ...lines.slice(0, -1),
      { ...last, text: last.text + delta, streaming: true },
    ];
  }
  const line: TranscriptLine =
    kind === "assistant"
      ? { kind: "assistant", text: delta, streaming: true }
      : { kind: "reasoning", text: delta, streaming: true };
  return [...lines, line];
}

// Mark any trailing streaming assistant/reasoning lines as finalized.
function finalizeStreaming(lines: readonly TranscriptLine[]): TranscriptLine[] {
  let i = lines.length;
  while (i > 0) {
    const l = lines[i - 1];
    if (
      l !== undefined &&
      (l.kind === "assistant" || l.kind === "reasoning") &&
      l.streaming
    ) {
      i -= 1;
    } else {
      break;
    }
  }
  if (i === lines.length) return [...lines];
  return [
    ...lines.slice(0, i),
    ...lines.slice(i).map((l) => ({ ...l, streaming: false })),
  ];
}

// Finalize trailing streaming, then push a new line.
function finalizeAndPush(
  lines: readonly TranscriptLine[],
  line: TranscriptLine,
): TranscriptLine[] {
  return [...finalizeStreaming(lines), line];
}

function proseOf(reason: FailoverChainExhaustion): string {
  return failoverChainExhaustionToProse(reason);
}

// The pure transcript reducer. Returns a new state; does not mutate.
export function applyEvent(
  state: TranscriptState,
  event: DomainEvent,
): TranscriptState {
  const lines = state.lines;
  switch (event.type) {
    case "busy":
      return { ...state, agentBusy: true };

    case "assistantContentDelta":
      return { ...state, lines: appendDelta(lines, "assistant", event.delta) };

    case "reasoningDelta":
      return { ...state, lines: appendDelta(lines, "reasoning", event.delta) };

    case "assistantContent":
      return {
        ...state,
        lines: finalizeAndPush(lines, {
          kind: "assistant",
          text: event.content,
        }),
      };

    case "reasoning":
      return {
        ...state,
        lines: finalizeAndPush(lines, {
          kind: "reasoning",
          text: event.content,
        }),
      };

    case "toolCall":
      return {
        ...state,
        lines: finalizeAndPush(lines, {
          kind: "toolCall",
          name: event.name,
          args: event.args,
        }),
      };

    case "toolResult":
      return {
        ...state,
        lines: finalizeAndPush(lines, {
          kind: "toolResult",
          result: event.result,
        }),
      };

    case "status":
      return {
        ...state,
        lines: [...lines, { kind: "status", text: event.message }],
      };

    case "error":
      return {
        ...state,
        agentBusy: false,
        lines: [...lines, { kind: "error", text: event.message }],
      };

    case "finished": {
      const last = lines[lines.length - 1];
      if (last && last.kind === "assistant" && last.streaming) {
        // Deltas already accumulated the content; just finalize.
        return { ...state, agentBusy: false, lines: finalizeStreaming(lines) };
      }
      return {
        ...state,
        agentBusy: false,
        lines: finalizeAndPush(lines, {
          kind: "assistant",
          text: event.content,
        }),
      };
    }

    case "retrying":
      return {
        ...state,
        lines: [
          ...lines,
          {
            kind: "retrying",
            attempt: event.attempt,
            maxAttempts: event.maxAttempts,
            error: event.error,
            delaySecs: event.delaySecs,
          },
        ],
      };

    case "failover":
      return {
        ...state,
        lines: [
          ...lines,
          {
            kind: "failover",
            from: event.from,
            to: event.to,
            reason: event.reason,
          },
        ],
      };

    case "failoverChainExhausted":
      return {
        ...state,
        agentBusy: false,
        lines: [
          ...lines,
          {
            kind: "failoverExhausted",
            reason: proseOf(event.reason),
            detail: event.detail,
          },
        ],
      };

    case "streamReset":
      return {
        ...state,
        lines: [
          ...finalizeStreaming(lines),
          {
            kind: "streamDropped",
            attempt: event.attempt,
            maxAttempts: event.maxAttempts,
            error: event.error,
            delaySecs: event.delaySecs,
          },
        ],
      };

    case "maxRoundsExceeded":
      return {
        ...state,
        agentBusy: false,
        lines: [...lines, { kind: "system", text: "max rounds exceeded" }],
      };

    case "tokenBudgetExceeded":
      return {
        ...state,
        agentBusy: false,
        lines: [
          ...lines,
          {
            kind: "system",
            text: `token budget exceeded (${event.consumed}/${event.budget})`,
          },
        ],
      };

    case "interrupted":
      return { ...state, agentBusy: false };

    case "cancelled":
      return { ...state, agentBusy: false };

    // approvalUpdated is handled by the approvals view, not the transcript.
    case "approvalUpdated":
      return state;

    default:
      return state;
  }
}
