// Vault primitive tests: real WebCrypto round trips (seal/open, tamper
// rejection, IV freshness) and the VaultKeyStore generate-or-load logic over
// an injected in-memory storage -- no browser or IndexedDB needed.

import { assert, assertEquals, assertRejects } from "@std/assert";
import {
  base64ToBytes,
  bytesToBase64,
  generateVaultKey,
  open,
  seal,
} from "./crypto.ts";
import { VaultKeyStore, type VaultKeyStorage } from "./keyStore.ts";

function memoryStorage(): VaultKeyStorage & { writes: number } {
  let slot: CryptoKey | null = null;
  return {
    writes: 0,
    get() {
      return Promise.resolve(slot);
    },
    set(key) {
      slot = key;
      this.writes++;
      return Promise.resolve();
    },
  };
}

Deno.test("seal/open: round trip returns the plaintext", async () => {
  const key = await generateVaultKey();
  const blob = await seal(key, "sk-live-abc123");
  assert(blob !== "sk-live-abc123");
  assertEquals(await open(key, blob), "sk-live-abc123");
});

Deno.test("open: rejects a blob sealed by another vault key", async () => {
  const [keyA, keyB] = await Promise.all([
    generateVaultKey(),
    generateVaultKey(),
  ]);
  const blob = await seal(keyA, "secret");
  await assertRejects(() => open(keyB, blob));
});

Deno.test("open: rejects a tampered blob (GCM auth)", async () => {
  const key = await generateVaultKey();
  const blob = await seal(key, "secret");
  const bytes = base64ToBytes(blob);
  bytes[bytes.length - 1] ^= 0xff;
  await assertRejects(() => open(key, bytesToBase64(bytes)));
});

Deno.test("open: rejects a truncated blob with no room for an iv", async () => {
  const key = await generateVaultKey();
  await assertRejects(() => open(key, bytesToBase64(new Uint8Array(4))));
});

Deno.test(
  "seal: fresh IV per call -- same input, different blobs",
  async () => {
    const key = await generateVaultKey();
    const first = await seal(key, "same-plaintext");
    const second = await seal(key, "same-plaintext");
    assert(first !== second);
    // Both still open to the same plaintext.
    assertEquals(await open(key, first), "same-plaintext");
    assertEquals(await open(key, second), "same-plaintext");
  },
);

Deno.test(
  "VaultKeyStore: generates once, persists, reuses across stores",
  async () => {
    const storage = memoryStorage();
    const first = new VaultKeyStore(storage);
    const keyA = await first.ensure();
    assertEquals(storage.writes, 1);

    // Same document instance: the cached promise wins, no second write.
    assertEquals(await first.ensure(), keyA);
    assertEquals(storage.writes, 1);

    // A new store over the same storage loads the persisted key (device
    // restart / page reload shape).
    const second = new VaultKeyStore(storage);
    assertEquals(await second.ensure(), keyA);
    assertEquals(storage.writes, 1);
  },
);
