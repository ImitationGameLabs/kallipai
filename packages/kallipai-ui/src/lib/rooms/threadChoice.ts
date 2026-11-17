// What the follow control says and does, for one thread's rule in force.
//
// One table for four answers: the words the control shows (the rule), the words
// for what a click does, the icon, and the write a click makes. It lives
// outside the component so the table can be tested without a DOM, and so the
// label and the action cannot drift into two accounts of one click. The
// accessible name is built from both, rule first, so it contains the words on
// screen (WCAG 2.5.3) instead of replacing them.
//
// The cycle reads off the current state alone, never off who is reading:
// following -> mute, muted -> drop the choice and take the thread's default,
// involved -> follow. Dropping the choice is the one step whose outcome belongs
// to the server (a human lands on following; an agent holding no choice lands
// on involved), which is why the control names the step rather than the state
// it lands on.
//
// Three states are not three choices for every reader: in the browser the
// reader is a human, whose default is following, so `involved` is drawn for
// completeness and never reached. Its copy says what the rule does in plain
// words; the wire word stays out of the interface.

import type {
  ThreadChoice,
  ThreadDelivery,
} from "@kallipai/kallipai-lesche-client";
import {
  room_thread_default_action,
  room_thread_follow_action,
  room_thread_following,
  room_thread_involved,
  room_thread_mute_action,
  room_thread_muted,
} from "../../paraglide/messages.js";

/** The icon drawn beside the words. */
export type ThreadChoiceIcon = "bell" | "bell-off" | "bell-ring";

export interface ThreadChoiceDisplay {
  /** The rule in force, in the reader's words. */
  readonly text: () => string;
  /** What a click does, in the reader's words (the button's accessible name). */
  readonly action: () => string;
  readonly icon: ThreadChoiceIcon;
  /** The choice a click records, or null when the click drops the row. */
  readonly next: ThreadChoice | null;
}

export function threadChoiceDisplay(
  effective: ThreadDelivery,
): ThreadChoiceDisplay {
  switch (effective) {
    case "following":
      return {
        text: room_thread_following,
        action: room_thread_mute_action,
        icon: "bell",
        next: "muted",
      };
    case "muted":
      return {
        text: room_thread_muted,
        action: room_thread_default_action,
        icon: "bell-off",
        next: null,
      };
    case "involved":
      return {
        text: room_thread_involved,
        action: room_thread_follow_action,
        icon: "bell-ring",
        next: "following",
      };
  }
}
