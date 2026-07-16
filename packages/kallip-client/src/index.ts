// @kallipai/kallip-client
//
// Direct daemon HTTP+SSE client. TypeScript counterpart to the Rust
// kallip-client crate: DaemonClient (fetch + authenticated SSE) and DaemonSession
// (implements @kallipai/kallip-common's Session for the UI).

export { DaemonClient } from "./client.ts";
export type { DaemonClientOptions } from "./client.ts";
export { DaemonSession } from "./session.ts";
export * from "./types.ts";
