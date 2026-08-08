// @kallipai/kallip-agora-client
//
// Browser client for the agora control-plane relay (default :7100): passkey
// register/login, `/me`, and the tagma lifecycle (mint/rename/revoke + the
// pinned device-key fetch). The data-plane client (conversations, key exchange,
// E2EE envelopes, app SSE) lives in `@kallipai/kallip-lesche-client`. The
// session cookie is shared cross-subdomain between agora and lesche.

export const PACKAGE_NAME = "@kallipai/kallip-agora-client";

export { AgoraClient } from "./http.ts";
export type { LoginBeginRequest, RegisterBeginRequest } from "./http.ts";
export {
  addPasskey,
  completeOAuth,
  completeOAuthSignup,
  loginWithDiscoverablePasskey,
  loginWithPasskey,
  pairDevice,
  registerWithPasskey,
  signInWithOAuth,
} from "./auth.ts";
export type {
  AddPasskeyArgs,
  AddPasskeyResult,
  CeremonyResult,
  OAuthCompleteResult,
  OAuthSignupResult,
  PairArgs,
  PairResult,
  RegisterArgs,
} from "./auth.ts";
export {
  loginCredentialToJson,
  optionsForCreate,
  optionsForGet,
  registerCredentialToJson,
} from "./webauthn.ts";
export type {
  AddEmailRequest,
  AddPasskeyFinishRequest,
  AuthFinishResponse,
  EmailSummary,
  ExternalIdentitySummary,
  LoginBeginResponse,
  LoginFinishRequest,
  MeResponse,
  MintPairingCodeResponse,
  MintTagmaResponse,
  OAuthBeginResponse,
  OAuthFinishRequest,
  OAuthNeedsUsernameResponse,
  OAuthSignupCompleteRequest,
  PairBeginRequest,
  PairFinishRequest,
  PasskeySummary,
  ProviderInfo,
  PublicTagmaProfile,
  PublicUserProfile,
  RegisterBeginResponse,
  RegisterFinishRequest,
  RenamePasskeyRequest,
  RenameTagmaRequest,
  TagmaState,
  TagmaView,
  VerifyEmailRequest,
} from "./types.ts";
export { AgoraApiError } from "./types.ts";
