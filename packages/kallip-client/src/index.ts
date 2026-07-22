// @kallipai/kallip-client
//
// Direct tagma HTTP+SSE client. TypeScript counterpart to the Rust
// kallip-client crate: TagmaClient (fetch + authenticated SSE) and TagmaSession
// (implements @kallipai/kallip-common's Session for the UI).

export { TagmaClient } from "./client.ts";
export type { TagmaClientOptions } from "./client.ts";
export { TagmaSession } from "./session.ts";
export * from "./types.ts";
