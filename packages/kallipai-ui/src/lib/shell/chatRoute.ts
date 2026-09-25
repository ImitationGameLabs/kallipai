/** The chat deep routes, parsed from the pathname. Three shapes exist:
 * the offline /local/chat (store key "local"), /chat/:id (the id IS the
 * conversation id), and /tagma/:id/chat (the id is a tagma id -- the
 * store keys conversations by the server-assigned conversation id, so
 * the shell resolves it through ChannelsStore before reading status).
 * Returns null off the chat routes; the guards reject empty ids and
 * ids containing a slash (a deeper path is not this route). Pure and
 * dependency-free so the derivation is unit-testable in isolation. */
export type ChatRoute =
  | { kind: "conversation"; conversationId: string }
  | { kind: "tagma"; tagmaId: string };

export function chatRoute(pathname: string): ChatRoute | null {
  if (pathname === "/local/chat") {
    return { kind: "conversation", conversationId: "local" };
  }
  if (pathname.startsWith("/chat/")) {
    const id = pathname.slice("/chat/".length);
    return id && !id.includes("/")
      ? { kind: "conversation", conversationId: id }
      : null;
  }
  if (pathname.startsWith("/tagma/") && pathname.endsWith("/chat")) {
    const tagmaId = pathname.slice("/tagma/".length, -"/chat".length);
    return tagmaId && !tagmaId.includes("/")
      ? { kind: "tagma", tagmaId }
      : null;
  }
  return null;
}
