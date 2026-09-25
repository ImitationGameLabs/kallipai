// manageChannelStalled: which no-conversation channel states can never
// produce a channel on their own. These tests pin the two stall forms so
// the manage page cannot get stuck on a permanent opening placeholder.

import { assertEquals } from "@std/assert";
import { manageChannelStalled } from "./channelStalled.ts";

Deno.test("absent stalls only once presence resolves without the peer", () => {
  assertEquals(manageChannelStalled({ kind: "absent" }, true), true);
  assertEquals(manageChannelStalled({ kind: "absent" }, false), false);
});

Deno.test(
  "unavailable stalls: the budget holds a failure, none in flight",
  () => {
    assertEquals(manageChannelStalled({ kind: "unavailable" }, false), true);
    assertEquals(manageChannelStalled({ kind: "unavailable" }, true), true);
  },
);

Deno.test(
  "states that may still open or hold a conversation do not stall",
  () => {
    assertEquals(
      manageChannelStalled({ kind: "pending", conversationId: "c" }, true),
      false,
    );
    assertEquals(
      manageChannelStalled({ kind: "open", conversationId: "c" }, true),
      false,
    );
    assertEquals(
      manageChannelStalled({ kind: "offline", conversationId: "c" }, true),
      false,
    );
    assertEquals(
      manageChannelStalled({ kind: "error", conversationId: "c" }, true),
      false,
    );
  },
);
