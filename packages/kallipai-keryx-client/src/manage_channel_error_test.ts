// The typed channel failure: the message names the call the caller was
// waiting on, and a caller that skips the detail (plain JS, or a cast past
// the overloads) still reads as an empty detail rather than "undefined".

import { assertEquals } from "@std/assert";
import { ManageChannelError } from "./channel.ts";

Deno.test("a timeout error names the call it was waiting on", () => {
  const named = new ManageChannelError("timeout", "POST /profiles/probe");
  assertEquals(named.reason, "timeout");
  assertEquals(named.message, "manage timeout: POST /profiles/probe");
});

Deno.test("a timeout with no detail does not read the word undefined", () => {
  const untypedCtor = ManageChannelError as unknown as new (
    reason: string,
  ) => ManageChannelError;
  const bare = new untypedCtor("timeout");
  assertEquals(bare.message, "manage timeout: ");
});

Deno.test("a closed channel keeps its own wording", () => {
  assertEquals(new ManageChannelError("closed").message, "channel closed");
});
