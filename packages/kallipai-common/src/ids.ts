// Opaque identifier types. On the wire these are bare JSON strings: UUID v4 for
// AgentId and the archeion ids. Modelled as plain string aliases; the wire adapters
// in @kallipai/kallipai-client and @kallipai/kallipai-archeion-client produce them.
import jsSHA from "jssha";

export type AgentId = string;
export type TagmaId = string;
export type ConversationId = string;
export type UserId = string;
export type TraceId = string;
export type SkillName = string;
export type ParticipantId = string;

// The opaque room-layer participant identity. Every room member -- a user, a
// tagma, or a future external agent -- is known to the room surface by this id.
// It is a deterministic v5 UUID derived from the underlying platform id, so it
// is stable across reconnects/restarts and requires no server round-trip. This
// MUST match the Rust derivation byte-for-byte
// (`crates/platform/kallipai-archeion-common/src/ids.rs`): v5 over the platform id's
// UUID *string* bytes (UTF-8), with a per-kind namespace constant. A mismatch
// would silently break the room envelope sender authentication + fan-out.

/** Namespace for `ParticipantId::for_user` (matches the Rust constant). */
const PARTICIPANT_FOR_USER_NAMESPACE = "b1f52a09-4ce7-4f1a-9d60-3e7c1b04a8f2";
/** Namespace for `ParticipantId::for_tagma` (matches the Rust constant). */
const PARTICIPANT_FOR_TAGMA_NAMESPACE = "c7a96e13-5db8-472e-ac41-2f8d9c15b7e1";

/** Parse a uuid string into 16 big-endian bytes (pairs of hex digits, `-`
 * stripped). */
function uuidToBytes(uuid: string): Uint8Array {
  const hex = uuid.replaceAll("-", "");
  const bytes = new Uint8Array(16);
  for (let i = 0; i < 16; i++) {
    bytes[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  return bytes;
}

/** Format 16 bytes as a lowercase uuid string. */
function bytesToUuid(bytes: Uint8Array): string {
  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join(
    "",
  );
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(
    16,
    20,
  )}-${hex.slice(20)}`;
}

/** SHA-1 over `data` for the v5 derivation. Prefers `crypto.subtle`
 * (present only in secure contexts); falls back to jssha where subtle
 * is undefined -- the plain-http LAN shape (http://kallipai.lan) has
 * no subtle, and without the fallback whoami / channel auto-open
 * break there. The `subtle` parameter is injectable so the dual-path
 * consistency test can execute the fallback branch directly. */
export async function sha1(
  data: Uint8Array<ArrayBuffer>,
  subtle: SubtleCrypto | undefined = crypto.subtle,
): Promise<Uint8Array> {
  if (subtle) {
    return new Uint8Array(await subtle.digest("SHA-1", data));
  }
  const hash = new jsSHA("SHA-1", "UINT8ARRAY");
  hash.update(data);
  return hash.getHash("UINT8ARRAY");
}

/** RFC 4122 v4 uuid. Prefers `crypto.randomUUID` (secure contexts only
 * per spec); falls back to getRandomValues -- available in every
 * context -- when the property is absent. The `randomUUID` parameter is
 * injectable so the fallback-path test can execute it directly. */
export function uuidV4(
  randomUUID: (() => string) | undefined = crypto.randomUUID?.bind(crypto),
): string {
  if (randomUUID) {
    return randomUUID();
  }
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  bytes[6] = (bytes[6]! & 0x0f) | 0x40;
  bytes[8] = (bytes[8]! & 0x3f) | 0x80;
  return bytesToUuid(bytes);
}

/** RFC 4122 v5 (SHA-1) uuid over `name` under `namespace`. */
async function uuidV5(namespace: string, name: string): Promise<string> {
  const data = new Uint8Array(16 + name.length);
  data.set(uuidToBytes(namespace), 0);
  data.set(new TextEncoder().encode(name), 16);
  const digest = (await sha1(data)).slice(0, 16);
  // Version 5 + variant bits (RFC 4122). `digest` is 16 bytes (sliced above),
  // so index 6 + 8 are in range; the `!` asserts that under noUncheckedIndexedAccess.
  digest[6] = (digest[6]! & 0x0f) | 0x50;
  digest[8] = (digest[8]! & 0x3f) | 0x80;
  return bytesToUuid(digest);
}

/** Derive the room-participant id for a user (mirrors Rust
 * `ParticipantId::for_user`). */
export function participantIdForUser(userId: UserId): Promise<ParticipantId> {
  return uuidV5(PARTICIPANT_FOR_USER_NAMESPACE, userId);
}

/** Derive the room-participant id for a tagma (mirrors Rust
 * `ParticipantId::for_tagma`). */
export function participantIdForTagma(
  tagmaId: TagmaId,
): Promise<ParticipantId> {
  return uuidV5(PARTICIPANT_FOR_TAGMA_NAMESPACE, tagmaId);
}
