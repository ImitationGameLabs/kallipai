// WebAuthn ceremony transforms: convert between the agora's JSON wire shapes
// (webauthn-rs serde -- every binary field is an UNPADDED base64url `String`)
// and the browser WebAuthn API (`BufferSource` in, `ArrayBuffer` out).
//
// Authoritative shapes (from the webauthn-rs-proto 0.6 source the agora
// serializes with): `CreationChallengeResponse = { publicKey: ... }`,
// `RequestChallengeResponse = { publicKey: ... }`, and the finish bodies
// `RegisterPublicKeyCredential` / `PublicKeyCredential` carry `rawId`,
// `attestationObject`/`clientDataJSON`/`transports` (register) and
// `authenticatorData`/`clientDataJSON`/`signature`/`userHandle` (login) as
// unpadded base64url strings. The browser counterpart of each is an
// `ArrayBuffer`, so we decode on the way in and encode on the way out.

import * as b64u from "./base64url.ts";

// ---------------------------------------------------------------------------
// Server -> browser: decode the challenge options the agora returned.
// ---------------------------------------------------------------------------

/** A credential descriptor as the agora serializes it (`id` is base64url). */
interface ServerCredentialDescriptor {
  readonly type: string;
  readonly id: string;
  readonly transports?: string[];
}

/** `CreationChallengeResponse.publicKey` (server JSON; binary fields are b64u). */
export interface ServerCreationOptions {
  readonly publicKey: {
    readonly rp: { readonly name: string; readonly id: string };
    readonly user: {
      readonly id: string;
      readonly name: string;
      readonly displayName: string;
    };
    readonly challenge: string;
    readonly pubKeyCredParams: ReadonlyArray<{
      readonly type: string;
      readonly alg: number;
    }>;
    readonly timeout?: number;
    readonly attestation?: string;
    readonly excludeCredentials?: ReadonlyArray<ServerCredentialDescriptor>;
    readonly authenticatorSelection?: Record<string, unknown>;
    readonly extensions?: Record<string, unknown>;
  };
}

/** `RequestChallengeResponse.publicKey` (server JSON; binary fields are b64u). */
export interface ServerRequestOptions {
  readonly publicKey: {
    readonly challenge: string;
    readonly timeout?: number;
    readonly rpId?: string;
    readonly allowCredentials?: ReadonlyArray<ServerCredentialDescriptor>;
    readonly userVerification?: string;
    readonly extensions?: Record<string, unknown>;
  };
}

function decodeDescriptors(
  descriptors: ReadonlyArray<ServerCredentialDescriptor> | undefined,
): PublicKeyCredentialDescriptor[] | undefined {
  if (!descriptors) return undefined;
  return descriptors.map((d) => ({
    type: d.type as PublicKeyCredentialType,
    id: b64u.decode(d.id),
    ...(d.transports
      ? { transports: d.transports as AuthenticatorTransport[] }
      : {}),
  }));
}

/** Build the `navigator.credentials.create({ publicKey })` argument. */
export function optionsForCreate(
  server: ServerCreationOptions,
): PublicKeyCredentialCreationOptions {
  const pk = server.publicKey;
  return {
    rp: pk.rp,
    user: { ...pk.user, id: b64u.decode(pk.user.id) },
    challenge: b64u.decode(pk.challenge),
    pubKeyCredParams: pk.pubKeyCredParams as PublicKeyCredentialParameters[],
    ...(pk.timeout !== undefined ? { timeout: pk.timeout } : {}),
    ...(pk.attestation
      ? { attestation: pk.attestation as AttestationConveyancePreference }
      : {}),
    ...(pk.excludeCredentials
      ? { excludeCredentials: decodeDescriptors(pk.excludeCredentials) }
      : {}),
    ...(pk.authenticatorSelection
      ? {
          authenticatorSelection:
            pk.authenticatorSelection as AuthenticatorSelectionCriteria,
        }
      : {}),
    ...(pk.extensions
      ? { extensions: pk.extensions as AuthenticationExtensionsClientInputs }
      : {}),
  };
}

/** Build the `navigator.credentials.get({ publicKey })` argument. */
export function optionsForGet(
  server: ServerRequestOptions,
): PublicKeyCredentialRequestOptions {
  const pk = server.publicKey;
  return {
    challenge: b64u.decode(pk.challenge),
    ...(pk.timeout !== undefined ? { timeout: pk.timeout } : {}),
    ...(pk.rpId ? { rpId: pk.rpId } : {}),
    ...(pk.allowCredentials
      ? { allowCredentials: decodeDescriptors(pk.allowCredentials) }
      : {}),
    ...(pk.userVerification
      ? { userVerification: pk.userVerification as UserVerificationRequirement }
      : {}),
    ...(pk.extensions
      ? { extensions: pk.extensions as AuthenticationExtensionsClientInputs }
      : {}),
  };
}

// ---------------------------------------------------------------------------
// Browser -> server: encode the credential the browser produced.
// ---------------------------------------------------------------------------

/**
 * The `POST /v1/auth/register/finish` body. Note `authenticatorData` is
 * absent: the agora's passkey register flow surfaces only `attestationObject`
 * + `clientDataJSON` (the `none` attestation path). A future richer-attestation
 * flow would add it here.
 */
export interface RegisterPublicKeyCredentialJson {
  readonly id: string;
  readonly rawId: string;
  readonly type: string;
  readonly response: {
    readonly attestationObject: string;
    readonly clientDataJSON: string;
    readonly transports: string[];
  };
  // webauthn-rs deserializes this via the `clientExtensionResults` alias.
  readonly clientExtensionResults: AuthenticationExtensionsClientOutputs;
}

/** The `POST /v1/auth/login/finish` body. */
export interface PublicKeyCredentialJson {
  readonly id: string;
  readonly rawId: string;
  readonly type: string;
  readonly response: {
    readonly authenticatorData: string;
    readonly clientDataJSON: string;
    readonly signature: string;
    readonly userHandle: string | null;
  };
  readonly clientExtensionResults: AuthenticationExtensionsClientOutputs;
}

function assertPublicKeyCredential(
  cred: PublicKeyCredential | null,
): asserts cred is PublicKeyCredential {
  if (!cred) {
    // The browser resolves `null` only when the user cancels via a path that
    // never produces a credential; the normal cancel surfaces as
    // `NotAllowedError` (handled by the caller). Treat null as a cancel too.
    throw new DOMException(
      "WebAuthn ceremony produced no credential",
      "NotAllowedError",
    );
  }
}

/** Encode a registration `PublicKeyCredential` for the finish body. */
export function registerCredentialToJson(
  cred: PublicKeyCredential | null,
): RegisterPublicKeyCredentialJson {
  assertPublicKeyCredential(cred);
  const response = cred.response as AuthenticatorAttestationResponse;
  // `getTransports`/`getClientExtensionResults` are function calls on the
  // credential/response, not properties -- a missing-call bug silently
  // deserializes to empty on the server.
  const transports = response.getTransports();
  const extensions = cred.getClientExtensionResults();
  return {
    id: cred.id,
    rawId: b64u.encode(cred.rawId),
    type: cred.type,
    response: {
      attestationObject: b64u.encode(response.attestationObject),
      clientDataJSON: b64u.encode(response.clientDataJSON),
      transports,
    },
    clientExtensionResults: extensions,
  };
}

/** Encode a login `PublicKeyCredential` for the finish body. */
export function loginCredentialToJson(
  cred: PublicKeyCredential | null,
): PublicKeyCredentialJson {
  assertPublicKeyCredential(cred);
  const response = cred.response as AuthenticatorAssertionResponse;
  const extensions = cred.getClientExtensionResults();
  // `userHandle` is nullable on the assertion response; null when absent.
  const userHandle = response.userHandle
    ? b64u.encode(response.userHandle)
    : null;
  return {
    id: cred.id,
    rawId: b64u.encode(cred.rawId),
    type: cred.type,
    response: {
      authenticatorData: b64u.encode(response.authenticatorData),
      clientDataJSON: b64u.encode(response.clientDataJSON),
      signature: b64u.encode(response.signature),
      userHandle,
    },
    clientExtensionResults: extensions,
  };
}
