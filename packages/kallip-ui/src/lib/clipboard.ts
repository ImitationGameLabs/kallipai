// Copy text via the async Clipboard API. Returns false on rejection or when the
// API is absent (older browsers, insecure context) so callers can skip the
// "copied" feedback instead of lying about success.
export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}
