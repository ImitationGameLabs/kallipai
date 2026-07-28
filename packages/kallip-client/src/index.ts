// @kallipai/kallip-client
//
// Direct tagma HTTP+SSE client. TypeScript counterpart to the Rust kallip-client
// crate: TagmaClient (fetch + authenticated SSE). The browser frontend consumes
// the external chat-room API via TagmaClient.externalEventStream.

export { TagmaClient } from "./client.ts";
export type { TagmaClientOptions } from "./client.ts";
export * from "./types.ts";
