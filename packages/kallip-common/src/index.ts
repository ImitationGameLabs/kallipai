// @kallipai/kallip-common
//
// Transport-agnostic shared layer: identifiers, errors, the unified DomainEvent
// union, the transcript model + reducer, SessionCapabilities, the Session
// interface, approval domain types, and a shared SSE parser.
// @kallipai/kallip-client implements Session; @kallipai/kallip-agora-client
// ships the agora HTTP + WebAuthn control-plane client today, with the
// Session-over-agora data plane a future phase. @kallipai/kallip-ui consumes
// these types.

export * from "./ids.ts";
export * from "./errors.ts";
export * from "./event.ts";
export * from "./approvals.ts";
export * from "./transcript.ts";
export * from "./capabilities.ts";
export * from "./session.ts";
export * from "./sse.ts";
