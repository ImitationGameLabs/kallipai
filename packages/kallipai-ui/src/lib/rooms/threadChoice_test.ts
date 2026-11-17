// The follow control's table: three rules, and what a click does in each.
// Compared against the message functions themselves, so the table can be read
// here without pinning any copy of it.

import { assertEquals } from "@std/assert";
import {
  room_thread_default_action,
  room_thread_follow_action,
  room_thread_following,
  room_thread_involved,
  room_thread_mute_action,
  room_thread_muted,
} from "../../paraglide/messages.js";
import { threadChoiceDisplay } from "./threadChoice.ts";

Deno.test("following names the rule and mutes on a click", () => {
  const display = threadChoiceDisplay("following");
  assertEquals(display.text, room_thread_following);
  assertEquals(display.action, room_thread_mute_action);
  assertEquals(display.icon, "bell");
  assertEquals(display.next, "muted");
});

Deno.test("muted drops the choice instead of writing a third state", () => {
  const display = threadChoiceDisplay("muted");
  assertEquals(display.text, room_thread_muted);
  assertEquals(display.action, room_thread_default_action);
  assertEquals(display.icon, "bell-off");
  assertEquals(display.next, null);
});

Deno.test("involved follows on a click and says it in plain words", () => {
  const display = threadChoiceDisplay("involved");
  assertEquals(display.text, room_thread_involved);
  assertEquals(display.action, room_thread_follow_action);
  assertEquals(display.icon, "bell-ring");
  assertEquals(display.next, "following");
  // The wire word stays out of the interface: a reader is told what the rule
  // does, not what it is called in the protocol.
  for (const words of [display.text(), display.action()]) {
    assertEquals(words.toLowerCase().includes("involved"), false, words);
  }
});

Deno.test("no two rules share a picture or a sentence", () => {
  const states = ["following", "muted", "involved"] as const;
  const icons = states.map((state) => threadChoiceDisplay(state).icon);
  const texts = states.map((state) => threadChoiceDisplay(state).text());
  assertEquals(new Set(icons).size, states.length);
  assertEquals(new Set(texts).size, states.length);
});
