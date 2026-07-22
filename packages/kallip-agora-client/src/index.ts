// @kallipai/kallip-agora-client
//
// Browser clients for the agora suite: `AgoraClient` (control plane — passkey
// register/login, `/me`, tagma lifecycle, pinned-key fetch) and `LescheClient`
// (data plane — conversations, key exchange, E2EE envelopes, app SSE). The
// package name predates the agora/lesche split; it covers both services' browser
// surfaces. Browser-first (session cookie auth shared cross-subdomain between
// agora and lesche; the WebAuthn transforms drive `navigator.credentials`).

export const PACKAGE_NAME = "@kallipai/kallip-agora-client";

export {
  AgoraClient,
  BaseClient,
  CSRF_HEADER,
  CSRF_HEADER_VALUE,
  LescheClient,
} from "./http.ts";
export type { LoginBeginRequest, RegisterBeginRequest } from "./http.ts";
export { loginWithPasskey, registerWithPasskey } from "./auth.ts";
export type { CeremonyResult, RegisterArgs } from "./auth.ts";
export {
  loginCredentialToJson,
  optionsForCreate,
  optionsForGet,
  registerCredentialToJson,
} from "./webauthn.ts";
export type {
  LoginBeginResponse,
  LoginFinishRequest,
  MeResponse,
  MintTagmaResponse,
  RegisterBeginResponse,
  RegisterFinishRequest,
  RenameTagmaRequest,
  TagmaState,
  TagmaView,
} from "./types.ts";
export { AgoraApiError } from "./types.ts";

// Chat data-plane (E2EE relay). Only the transport surface is re-exported; the
// crypto primitives in ./crypto.ts are internals (tests import them directly).
export type {
  AgoraEvent,
  CreateConversationResponse,
  Envelope,
  FailoverChainExhaustion,
  KeyExchangeInit,
  KeyExchangeResponse,
  Participant,
  TagmaEvent,
  TagmaInfo,
  TagmaReply,
  TagmaRequest,
} from "./types.ts";
export { decodeB64, encodeB64 } from "./base64.ts";
export { RelayChannel, openRelayChannel } from "./channel.ts";
