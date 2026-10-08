// @kallipai/kallipai-common
//
// Transport-agnostic shared layer: identifiers, errors, the standard base64
// wire codec, a shared SSE parser, and the data-plane fetch discipline the
// relay clients share. The frontend talks to the tagma through
// transport-specific clients (@kallipai/kallipai-client for the direct path,
// @kallipai/kallipai-lesche-client / -keryx-client for the relayed paths)
// rather than a shared session contract.
export * from "./ids.ts";
export * from "./errors.ts";
export * from "./base64.ts";
export * from "./sse.ts";
export * from "./chat.ts";
export * from "./http.ts";
