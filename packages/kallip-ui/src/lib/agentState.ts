// Six-state presentation table shared by every state-bearing surface --
// the manage list + detail header (StateDot), the top-bar pills, the
// detail layer, and the status card's agent rows and drawer. The union
// mirrors the tagma's `AgentState` wire enum (see
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

/** Lucide icon for every state-bearing surface: shape + motion + color
 * (never color alone, colorblind-safe). Rows and the drawer sit on the
 * status bar's 200-800 tone, not the page bg — stock 600-400 pairs
 * nearly vanish there (warning-600 is ~2 ΔL oklab on the light bar) —
 * so these draw the stock deeper pairs (700-300; warning 800-200),
 * already defined by the Skeleton base theme. idle inverts the 400-600
 * pair so each mode draws the half further from the bar tone (600 off
 * the light bar, 400 off the dark). On top of motion, the three live
 * states stay shape-distinct when motion-reduce pauses the animation:
 * busy draws a solid ring, waiting a dashed one, retrying a ring with
 * the centered mark. */
export interface StateIconSpec {
  readonly comp: typeof Circle;
  /** Optional centered overlay mark: retrying draws "!" inside the
   * spinning ring, the same shape language as the parked glyph, so
   * motion-reduce environments can still tell it apart from busy's
   * plain ring (solid) and waiting's dashed one. */
  readonly center?: string;
  /** Tone-pair classes; shared by the icon and its centered overlay. */
  readonly colorClassName: string;
  /** Motion classes for the icon itself; the overlay never takes them
   * (retrying's "!" must stay still inside the spinning ring). */
  readonly motionClassName?: string;
}

const ICONS: Record<AgentLifecycleState, StateIconSpec> = {
  idle: { comp: Circle, colorClassName: "text-surface-600-400" },
  busy: {
    comp: LoaderCircle,
    colorClassName: "text-success-700-300",
    motionClassName: "animate-spin",
  },
  waiting: {
    comp: CircleDashed,
    colorClassName: "text-primary-700-300",
    motionClassName: "animate-pulse",
  },
  retrying: {
    comp: LoaderCircle,
    colorClassName: "text-warning-800-200",
    motionClassName: "animate-spin",
    center: "!",
  },
  parked: { comp: TriangleAlert, colorClassName: "text-warning-800-200" },
  faulted: { comp: CircleX, colorClassName: "text-error-700-300" },
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
