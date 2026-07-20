// @kallipai/kallip-agora-client
//
// HTTP + WebAuthn ceremony client for the agora `/v1` control-plane surface:
// passkey register/login, `/me` profile, and the owner's tagmata across their
// pending -> enrolled -> revoked lifecycle. Browser-first (session cookie auth;
// the WebAuthn transforms drive `navigator.credentials`). The Session-over-agora
// data-plane (E2EE relay, SSE) is a future phase and not implemented here.

export const PACKAGE_NAME = "@kallipai/kallip-agora-client";

export { AgoraClient, CSRF_HEADER, CSRF_HEADER_VALUE } from "./http.ts";
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
