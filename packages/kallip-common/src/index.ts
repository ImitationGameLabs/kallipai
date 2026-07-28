// @kallipai/kallip-common
//
// Transport-agnostic shared layer: identifiers, errors, and a shared SSE
// parser. The frontend talks to the tagma through transport-specific clients
// (@kallipai/kallip-client for the direct path, @kallipai/kallip-lesche-client
// for the relayed path) rather than a shared session contract.

export * from "./ids.ts";
export * from "./errors.ts";
export * from "./sse.ts";
export * from "./chat.ts";
