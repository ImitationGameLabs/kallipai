// @kallipai/kallipai-client
//
// Direct tagma HTTP+SSE client. TypeScript counterpart to the Rust kallipai-client
// crate: TagmaClient (fetch + authenticated SSE). The browser frontend consumes
// the external chat-room API via TagmaClient.externalEventStream.
// Each verb's wire shape is pinned in src/tagma-route-shapes_test.ts.

export { TagmaClient } from "./client.ts";
export type { TagmaClientOptions } from "./client.ts";
export * from "./types.ts";
