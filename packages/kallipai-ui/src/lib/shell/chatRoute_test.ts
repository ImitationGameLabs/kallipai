import { assert, assertEquals } from "@std/assert";

import { chatRoute } from "./chatRoute.ts";

// chatRoute carries the shell's conversation-key derivation. The
// /tagma/:id/chat shape must NOT yield the tagma id as a conversation
// id: the store keys conversations by the server-assigned conversation
// id (ChannelsStore.conversationIdForTagma resolves it), and handing the
// tagma id to the store reads nothing forever -- the regression this
// pins (the mobile top row showed "waiting" on a live tagma).

Deno.test("chatRoute maps /local/chat to the local conversation", () => {
  assertEquals(chatRoute("/local/chat"), {
    kind: "conversation",
    conversationId: "local",
  });
});

Deno.test("chatRoute carries /chat/:id as the conversation id", () => {
  assertEquals(chatRoute("/chat/conv-123"), {
    kind: "conversation",
    conversationId: "conv-123",
  });
});

Deno.test("chatRoute yields the tagma shape for /tagma/:id/chat", () => {
  assertEquals(chatRoute("/tagma/tagma-9/chat"), {
    kind: "tagma",
    tagmaId: "tagma-9",
  });
});

Deno.test("chatRoute guards reject empty and slash-bearing ids", () => {
  assertEquals(chatRoute("/chat/"), null);
  assertEquals(chatRoute("/chat/a/b"), null);
  assertEquals(chatRoute("/tagma//chat"), null);
  assertEquals(chatRoute("/tagma/a/b/chat"), null);
  assertEquals(chatRoute("/tagma/chat"), null);
});

Deno.test("chatRoute is null off the chat routes", () => {
  assertEquals(chatRoute("/chats"), null);
  assertEquals(chatRoute("/settings"), null);
  assertEquals(chatRoute("/tagmata"), null);
  assert((chatRoute("/tagma/tagma-9/details") !== null) === false);
});
