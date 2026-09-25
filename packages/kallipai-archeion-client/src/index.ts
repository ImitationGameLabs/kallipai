// @kallipai/kallipai-archeion-client
//
// Browser client for the archeion control-plane relay (default :7100): passkey
// register/login, `/me`, and the tagma lifecycle (mint/rename/revoke + the
// pinned device-key fetch). The data-plane client (conversations, key exchange,
// E2EE envelopes, app SSE) lives in `@kallipai/kallipai-lesche-client`. The
// session cookie is shared cross-subdomain between archeion and lesche.

export const PACKAGE_NAME = "@kallipai/kallipai-archeion-client";

export { ArcheionClient } from "./http.ts";
export type { LoginBeginRequest, RegisterBeginRequest } from "./http.ts";
export {
  addPasskey,
  adminLoginWithKey,
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
  AdminLoginResult,
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
  ProviderKeyMode,
  ProviderRequest,
  ProviderSummary,
  ProviderInfo,
  PublicTagmaProfile,
  PublicUserProfile,
  RegisterBeginResponse,
  RegisterFinishRequest,
  RenamePasskeyRequest,
  RenameTagmaRequest,
  TagmaState,
  UsernameAvailabilityResponse,
  UsernameAvailabilityStatus,
  TagmaView,
  VerifyEmailRequest,
} from "./types.ts";
export { ArcheionApiError } from "./types.ts";
