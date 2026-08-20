// Six-state presentation table shared by every state-bearing surface: the
// manage list + detail header (StateDot, word), and the status card's agent
// rows. The union mirrors the tagma's `AgentState` wire enum (see
// @kallipai/kallip-client/src/types.ts) — copied, not imported, so this
// module stays transport-free like tagmata.svelte.ts.

import {
  agent_state_busy,
  agent_state_faulted,
  agent_state_idle,
  agent_state_parked,
  agent_state_retrying,
  agent_state_waiting,
} from "../paraglide/messages.js";
import {
  Circle,
  CircleDashed,
  CircleX,
  LoaderCircle,
  TriangleAlert,
} from "@lucide/svelte";

export type AgentLifecycleState =
  | "idle"
  | "busy"
  | "waiting"
  | "retrying"
  | "parked"
  | "faulted";

/** Textured glyph for a state dot (manage list + detail header): shape +
 * color (never color alone), with `waiting` breathing so a stalled-looking
 * agent still reads as alive. Dots sit on the page background (white/
 * 950), where the stock 600-400 semantic pairs hold contrast; `waiting`
 * draws primary because the theme ships no info ramp — the old
 * text-info-500 class resolved to nothing (icons silently inherited the
 * body color). */
export interface StateGlyph {
  readonly char: "◌" | "●" | "▲" | "✕";
  readonly className: string;
}

const GLYPHS: Record<AgentLifecycleState, StateGlyph> = {
  idle: { char: "◌", className: "text-surface-400-600" },
  busy: { char: "●", className: "text-success-600-400" },
  waiting: { char: "●", className: "text-primary-600-400 animate-pulse" },
  retrying: { char: "●", className: "text-warning-600-400" },
  parked: { char: "▲", className: "text-warning-600-400" },
  faulted: { char: "✕", className: "text-error-600-400" },
};

export function agentStateGlyph(state: AgentLifecycleState): StateGlyph {
  return GLYPHS[state];
}

/** Lucide icon for the agent rows: shape + motion + color (never color
 * alone, colorblind-safe). The rows sit on the status bar's 200-800 tone,
 * not the page bg — stock 600-400 pairs nearly vanish there (warning-600
 * is ~2 ΔL oklab on the light bar) — so these draw the stock deeper
 * pairs (700-300; warning 800-200), already defined by the Skeleton
 * base theme. idle inverts the glyph's 400-600 so each mode draws the
 * half further from the bar tone (600 off the light bar, 400 off the
 * dark). */
export interface StateIconSpec {
  readonly comp: typeof Circle;
  readonly className: string;
}

const ICONS: Record<AgentLifecycleState, StateIconSpec> = {
  idle: { comp: Circle, className: "text-surface-600-400" },
  busy: { comp: LoaderCircle, className: "animate-spin text-success-700-300" },
  waiting: {
    comp: CircleDashed,
    className: "animate-pulse text-primary-700-300",
  },
  retrying: {
    comp: LoaderCircle,
    className: "animate-spin text-warning-800-200",
  },
  parked: { comp: TriangleAlert, className: "text-warning-800-200" },
  faulted: { comp: CircleX, className: "text-error-700-300" },
};

export function agentStateIcon(state: AgentLifecycleState): StateIconSpec {
  return ICONS[state];
}

export function agentStateLabel(state: AgentLifecycleState): string {
  switch (state) {
    case "idle":
      return agent_state_idle();
    case "busy":
      return agent_state_busy();
    case "waiting":
      return agent_state_waiting();
    case "retrying":
      return agent_state_retrying();
    case "parked":
      return agent_state_parked();
    case "faulted":
      return agent_state_faulted();
  }
}
