// Remembered conversation-id mappings: the last conversation id each
// conversation partition (tagma, or the direct "local" partition) resolved
// to, so an offline open can hydrate the cached transcript without a server
// round trip (the relay conversation id is server-derived and otherwise
// unknown offline). A derived id -- no content -- so localStorage is an
// acceptable home; best-effort both ways. Pure module: no runes, so the
// reverse-resolve helper stays unit-testable under plain deno test.

export const CONV_OF_PREFIX = "kallip-relay:conv-of:";

export function convOfKey(partition: string): string {
  return CONV_OF_PREFIX + partition;
}

export function rememberConversationOf(
  partition: string,
  conversationId: string,
): void {
  try {
    localStorage.setItem(convOfKey(partition), conversationId);
  } catch {
    // storage blocked: the offline view just starts empty
  }
}

export function lastConversationOf(partition: string): string | undefined {
  try {
    return localStorage.getItem(convOfKey(partition)) ?? undefined;
  } catch {
    return undefined;
  }
}

/** Reverse-resolve the tagma that owns `conversationId`, by scanning the
 * remembered conv-of keys. Undefined when no known tagma ever opened it.
 * Lets a /chat/{id} deep-link find its tagma even when the channel never
 * opened (offline tagma), so the page can mount the degraded view. */
export function tagmaForConversation(
  conversationId: string,
): string | undefined {
  try {
    for (let i = 0; i < localStorage.length; i++) {
      const key = localStorage.key(i);
      if (key === null || !key.startsWith(CONV_OF_PREFIX)) continue;
      if (localStorage.getItem(key) === conversationId) {
        const partition = key.slice(CONV_OF_PREFIX.length);
        if (partition === "local") continue; // not a tagma id
        return partition;
      }
    }
  } catch {
    // storage blocked: nothing remembered to reverse-resolve
  }
  return undefined;
}
