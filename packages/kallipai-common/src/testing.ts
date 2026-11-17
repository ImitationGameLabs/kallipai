// Test-only helpers shared by the client packages. Imported as
// `@kallipai/kallipai-common/testing`; no runtime module imports it, so it
// stays out of the browser bundle.

/**
 * Swap `globalThis.fetch` for the duration of `run`, restoring the previous
 * implementation even when `run` throws. The restore lives in here, so a
 * failed assertion cannot leak a stub into the next test.
 */
export async function withFetch<T>(
  stub: (input: string | URL, init?: RequestInit) => Promise<Response>,
  run: () => T | Promise<T>,
): Promise<T> {
  const original = globalThis.fetch;
  globalThis.fetch = stub as typeof fetch;
  try {
    return await run();
  } finally {
    globalThis.fetch = original;
  }
}
