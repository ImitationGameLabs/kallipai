// Vault primitives for the user-provider encrypted mode.
//
// The browser generates one AES-GCM-256 vault key per origin and keeps it in
// IndexedDB (never extractable, never sent anywhere). Encrypting a provider
// key means sealing it under that device-held key; the archeion stores the blob
// opaquely -- it can neither read nor decrypt `encrypted` rows. The wire
// format is base64(iv ‖ ciphertext): 12 random bytes of IV prepended to the
// GCM ciphertext (the tag rides inside it), one string for the TEXT column.

const IV_LENGTH = 12;
const ALGORITHM = "AES-GCM";

/** Generate the per-device vault key. Non-extractable by construction: the
 *  key material stays inside the browser's keystore. */
export function generateVaultKey(): Promise<CryptoKey> {
  return crypto.subtle.generateKey({ name: ALGORITHM, length: 256 }, false, [
    "encrypt",
    "decrypt",
  ]);
}

export function bytesToBase64(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
}

export function base64ToBytes(text: string): Uint8Array {
  const binary = atob(text);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

/** Seal a plaintext key into the stored blob form (base64 of iv ‖ ct). A fresh
 *  random IV on every call, so sealing the same key twice yields different
 *  blobs. */
export async function seal(
  vaultKey: CryptoKey,
  plaintext: string,
): Promise<string> {
  const iv = crypto.getRandomValues(new Uint8Array(IV_LENGTH));
  const ciphertext = await crypto.subtle.encrypt(
    { name: ALGORITHM, iv },
    vaultKey,
    new TextEncoder().encode(plaintext),
  );
  const blob = new Uint8Array(iv.length + ciphertext.byteLength);
  blob.set(iv);
  blob.set(new Uint8Array(ciphertext), iv.length);
  return bytesToBase64(blob);
}

/** Open a stored blob back to the plaintext key. Throws when the vault key is
 *  not the one that sealed the blob (or the blob was tampered with) -- GCM
 *  authentication fails rather than returning garbage. */
export async function open(vaultKey: CryptoKey, blob: string): Promise<string> {
  const bytes = base64ToBytes(blob);
  if (bytes.length <= IV_LENGTH) {
    throw new Error("provider blob too short to contain an iv");
  }
  const iv = bytes.slice(0, IV_LENGTH);
  const plaintext = await crypto.subtle.decrypt(
    { name: ALGORITHM, iv },
    vaultKey,
    bytes.slice(IV_LENGTH),
  );
  return new TextDecoder().decode(plaintext);
}
